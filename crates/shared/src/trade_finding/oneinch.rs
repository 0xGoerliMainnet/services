//! A 1Inch-based trade finder.

use {
    super::{Interaction, Query, Quote, Trade, TradeError, TradeFinding},
    crate::{
        oneinch_api::{
            Cache,
            OneInchClient,
            OneInchError,
            SellOrderQuoteQuery,
            Slippage,
            Swap,
            SwapQuery,
        },
        price_estimation::gas,
        request_sharing::{BoxRequestSharing, BoxShared, RequestSharing},
    },
    futures::FutureExt as _,
    model::order::OrderKind,
    primitive_types::H160,
    std::sync::Arc,
};

pub struct OneInchTradeFinder {
    inner: Arc<Inner>,
    sharing: BoxRequestSharing<InternalQuery, Result<Quote, TradeError>>,
}

struct Inner {
    api: Arc<dyn OneInchClient>,
    disabled_protocols: Vec<String>,
    cache: Cache,
    referrer_address: Option<H160>,
    solver: H160,
    settlement_contract: H160,
}

#[derive(Clone, Eq, PartialEq)]
struct InternalQuery {
    data: Query,
    allowed_protocols: Option<Vec<String>>,
}

impl OneInchTradeFinder {
    pub fn new(
        api: Arc<dyn OneInchClient>,
        disabled_protocols: Vec<String>,
        referrer_address: Option<H160>,
        solver: H160,
        settlement_contract: H160,
    ) -> Self {
        Self {
            inner: Arc::new(Inner::new(
                api,
                disabled_protocols,
                referrer_address,
                solver,
                settlement_contract,
            )),
            sharing: RequestSharing::labelled("oneinch".into()),
        }
    }

    fn shared_quote(
        &self,
        query: &Query,
        allowed_protocols: Option<Vec<String>>,
    ) -> BoxShared<Result<Quote, TradeError>> {
        let query = InternalQuery {
            data: query.clone(),
            allowed_protocols,
        };

        self.sharing.shared_or_else(query, move |query| {
            let inner = self.inner.clone();
            let query = query.clone();
            async move { inner.perform_quote(query).await }.boxed()
        })
    }

    async fn quote(&self, query: &Query) -> Result<Quote, TradeError> {
        let allowed_protocols = self.inner.verify_query_and_get_protocols(query).await?;
        self.shared_quote(query, allowed_protocols).await
    }

    async fn swap(&self, query: &Query) -> Result<Trade, TradeError> {
        let allowed_protocols = self.inner.verify_query_and_get_protocols(query).await?;
        let (quote, spender, swap) = futures::try_join!(
            self.shared_quote(query, allowed_protocols.clone()),
            self.inner.spender(),
            self.inner.swap(query, allowed_protocols),
        )?;

        Ok(Trade::swap(
            query.sell_token,
            quote.out_amount,
            quote.gas_estimate,
            Some(spender),
            Interaction {
                target: swap.tx.to,
                value: swap.tx.value,
                data: swap.tx.data,
            },
            self.inner.solver,
        ))
    }
}

impl Inner {
    fn new(
        api: Arc<dyn OneInchClient>,
        disabled_protocols: Vec<String>,
        referrer_address: Option<H160>,
        solver: H160,
        settlement_contract: H160,
    ) -> Self {
        Self {
            api,
            disabled_protocols,
            referrer_address,
            cache: Default::default(),
            solver,
            settlement_contract,
        }
    }

    async fn verify_query_and_get_protocols(
        &self,
        query: &Query,
    ) -> Result<Option<Vec<String>>, TradeError> {
        if query.kind == OrderKind::Buy {
            return Err(TradeError::UnsupportedOrderType("buy order".to_string()));
        }

        let allowed_protocols = self
            .cache
            .allowed_protocols(&self.disabled_protocols, self.api.as_ref())
            .await?;

        Ok(allowed_protocols)
    }

    async fn perform_quote(&self, query: InternalQuery) -> Result<Quote, TradeError> {
        let quote = self
            .api
            .get_sell_order_quote(SellOrderQuoteQuery::with_default_options(
                query.data.sell_token,
                query.data.buy_token,
                query.allowed_protocols,
                query.data.in_amount.get(),
                self.referrer_address,
            ))
            .await?;

        Ok(Quote {
            out_amount: quote.to_token_amount,
            gas_estimate: gas::SETTLEMENT_OVERHEAD + quote.estimated_gas,
            solver: self.solver,
        })
    }

    /// Returns the current 1Inch smart contract as the `spender`.
    async fn spender(&self) -> Result<H160, TradeError> {
        let spender = self.cache.spender(self.api.as_ref()).await?;
        Ok(spender.address)
    }

    async fn swap(
        &self,
        query: &Query,
        allowed_protocols: Option<Vec<String>>,
    ) -> Result<Swap, TradeError> {
        Ok(self
            .api
            .get_swap(SwapQuery::with_default_options(
                query.sell_token,
                query.buy_token,
                query.in_amount.get(),
                self.settlement_contract,
                allowed_protocols,
                Slippage::ONE_PERCENT,
                self.referrer_address,
            ))
            .await?)
    }
}

impl From<OneInchError> for TradeError {
    fn from(err: OneInchError) -> Self {
        match err {
            OneInchError::Api(err) if err.status_code == 429 => Self::RateLimited,
            err if err.is_insuffucient_liquidity() => Self::NoLiquidity,
            err => Self::Other(err.into()),
        }
    }
}

#[async_trait::async_trait]
impl TradeFinding for OneInchTradeFinder {
    async fn get_quote(&self, query: &Query) -> Result<Quote, TradeError> {
        self.quote(query).await
    }

    async fn get_trade(&self, query: &Query) -> Result<Trade, TradeError> {
        self.swap(query).await
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::oneinch_api::{
            MockOneInchClient,
            OneInchClientImpl,
            RestError,
            SellOrderQuote,
            Spender,
            Swap,
            Token,
            Transaction,
        },
        hex_literal::hex,
        number::nonzero::U256 as NonZeroU256,
        reqwest::Client,
        std::time::Duration,
    };

    fn create_trade_finder<T: OneInchClient>(api: T) -> OneInchTradeFinder {
        OneInchTradeFinder::new(
            Arc::new(api),
            Vec::default(),
            None,
            H160([1; 20]),
            H160([2; 20]),
        )
    }

    #[tokio::test]
    async fn quote_sell_order_succeeds() {
        // How much GNO can you buy for 1 WETH
        let mut one_inch = MockOneInchClient::new();

        // Response was generated with:
        //
        // curl 'https://api.1inch.io/v4.0/1/quote?\
        //     fromTokenAddress=0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2&\
        //     toTokenAddress=0x6810e776880c02933d47db1b9fc05908e5386b96&\
        //     amount=100000000000000000'
        one_inch.expect_get_sell_order_quote().return_once(|_| {
            async {
                Ok(SellOrderQuote {
                    from_token: Token {
                        address: testlib::tokens::WETH,
                    },
                    to_token: Token {
                        address: testlib::tokens::GNO,
                    },
                    to_token_amount: 808_069_760_400_778_577u128.into(),
                    from_token_amount: 100_000_000_000_000_000u128.into(),
                    protocols: Vec::default(),
                    estimated_gas: 189_386,
                })
            }
            .boxed()
        });

        let estimator = create_trade_finder(one_inch);

        let quote = estimator
            .get_quote(&Query {
                verification: None,
                sell_token: testlib::tokens::WETH,
                buy_token: testlib::tokens::GNO,
                in_amount: NonZeroU256::try_from(1_000_000_000_000_000_000u128).unwrap(),
                kind: OrderKind::Sell,
            })
            .await
            .unwrap();

        assert_eq!(quote.out_amount, 808_069_760_400_778_577u128.into());
        assert!(quote.gas_estimate > 189_386);
    }

    #[tokio::test]
    async fn estimate_sell_order_succeeds() {
        // How much GNO can you buy for 1 WETH
        let mut one_inch = MockOneInchClient::new();

        // Response was generated with:
        //
        // curl 'https://api.1inch.io/v4.0/1/quote?\
        //     fromTokenAddress=0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2&\
        //     toTokenAddress=0x6810e776880c02933d47db1b9fc05908e5386b96&\
        //     amount=100000000000000000'
        //
        // curl 'https://api.1inch.io/v4.0/1/swap?\
        //     fromTokenAddress=0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2&\
        //     toTokenAddress=0x6810e776880c02933d47db1b9fc05908e5386b96&\
        //     amount=100000000000000000&\
        //     fromAddress=0x0000000000000000000000000000000000000000&\
        //     slippage=1&\
        //     disableEstimate=true'
        one_inch.expect_get_sell_order_quote().return_once(|_| {
            async {
                Ok(SellOrderQuote {
                    from_token: Token {
                        address: testlib::tokens::WETH,
                    },
                    to_token: Token {
                        address: testlib::tokens::GNO,
                    },
                    to_token_amount: 808_069_760_400_778_577u128.into(),
                    from_token_amount: 100_000_000_000_000_000u128.into(),
                    protocols: Vec::default(),
                    estimated_gas: 189_386,
                })
            }
            .boxed()
        });
        one_inch.expect_get_spender().return_once(|| {
            async {
                Ok(Spender {
                    address: addr!("11111112542d85b3ef69ae05771c2dccff4faa26"),
                })
            }
            .boxed()
        });
        one_inch.expect_get_swap().return_once(|_| {
            async {
                Ok(Swap {
                    from_token: Token {
                        address: testlib::tokens::WETH,
                    },
                    to_token: Token {
                        address: testlib::tokens::GNO,
                    },
                    to_token_amount: 808_069_760_400_778_577u128.into(),
                    from_token_amount: 100_000_000_000_000_000u128.into(),
                    protocols: Default::default(),
                    tx: Transaction {
                        from: Default::default(),
                        to: addr!("1111111254fb6c44bac0bed2854e76f90643097d"),
                        data: vec![0xe4, 0x49, 0x02, 0x2e],
                        value: Default::default(),
                        gas_price: Default::default(),
                        gas: Default::default(),
                    },
                })
            }
            .boxed()
        });

        let estimator = create_trade_finder(one_inch);

        let trade = estimator
            .get_trade(&Query {
                verification: None,
                sell_token: testlib::tokens::WETH,
                buy_token: testlib::tokens::GNO,
                in_amount: NonZeroU256::try_from(1_000_000_000_000_000_000u128).unwrap(),
                kind: OrderKind::Sell,
            })
            .await
            .unwrap();

        assert_eq!(trade.out_amount, 808_069_760_400_778_577u128.into());
        assert!(trade.gas_estimate > 189_386);
        assert_eq!(
            trade.interactions,
            vec![
                Interaction {
                    target: testlib::tokens::WETH,
                    value: 0.into(),
                    data: hex!(
                        "095ea7b3
                         00000000000000000000000011111112542d85b3ef69ae05771c2dccff4faa26
                         0000000000000000000000000000000000000000000000000000000000000000"
                    )
                    .to_vec(),
                },
                Interaction {
                    target: testlib::tokens::WETH,
                    value: 0.into(),
                    data: hex!(
                        "095ea7b3
                         00000000000000000000000011111112542d85b3ef69ae05771c2dccff4faa26
                         ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    )
                    .to_vec(),
                },
                Interaction {
                    target: addr!("1111111254fb6c44bac0bed2854e76f90643097d"),
                    value: Default::default(),
                    data: vec![0xe4, 0x49, 0x02, 0x2e],
                },
            ]
        );
    }

    #[tokio::test]
    async fn estimating_buy_order_fails() {
        let mut one_inch = MockOneInchClient::new();

        one_inch.expect_get_sell_order_quote().times(0);

        let estimator = create_trade_finder(one_inch);

        let est = estimator
            .get_trade(&Query {
                verification: None,
                sell_token: testlib::tokens::WETH,
                buy_token: testlib::tokens::GNO,
                in_amount: NonZeroU256::try_from(1_000_000_000_000_000_000u128).unwrap(),
                kind: OrderKind::Buy,
            })
            .await;

        assert!(matches!(est, Err(TradeError::UnsupportedOrderType(_))));
    }

    #[tokio::test]
    async fn rest_api_errors_get_propagated() {
        let mut one_inch = MockOneInchClient::new();
        one_inch
            .expect_get_sell_order_quote()
            .times(1)
            .return_once(|_| {
                async {
                    Err(OneInchError::Api(RestError {
                        status_code: 500,
                        description: "Internal Server Error".to_string(),
                    }))
                }
                .boxed()
            });

        let estimator = create_trade_finder(one_inch);

        let est = estimator
            .get_trade(&Query {
                verification: None,
                sell_token: testlib::tokens::WETH,
                buy_token: testlib::tokens::GNO,
                in_amount: NonZeroU256::try_from(1_000_000_000_000_000_000u128).unwrap(),
                kind: OrderKind::Sell,
            })
            .await;

        assert!(matches!(
            est,
            Err(TradeError::Other(e)) if e.to_string().contains("Internal Server Error")
        ));
    }

    #[tokio::test]
    async fn request_errors_get_propagated() {
        let mut one_inch = MockOneInchClient::new();
        one_inch
            .expect_get_sell_order_quote()
            .times(1)
            .return_once(|_| {
                async { Err(OneInchError::Other(anyhow::anyhow!("malformed JSON"))) }.boxed()
            });

        let estimator = create_trade_finder(one_inch);

        let est = estimator
            .get_trade(&Query {
                verification: None,
                sell_token: testlib::tokens::WETH,
                buy_token: testlib::tokens::GNO,
                in_amount: NonZeroU256::try_from(1_000_000_000_000_000_000u128).unwrap(),
                kind: OrderKind::Sell,
            })
            .await;

        assert!(matches!(
            est,
            Err(TradeError::Other(e)) if e.to_string() == "malformed JSON"
        ));
    }

    #[tokio::test]
    async fn shares_quote_api_request() {
        let mut oneinch = MockOneInchClient::new();
        oneinch.expect_get_sell_order_quote().return_once(|_| {
            async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Ok(Default::default())
            }
            .boxed()
        });
        oneinch
            .expect_get_spender()
            .return_once(|| async { Ok(Default::default()) }.boxed());
        oneinch
            .expect_get_swap()
            .return_once(|_| async { Ok(Default::default()) }.boxed());

        let trader = OneInchTradeFinder::new(
            Arc::new(oneinch),
            Vec::new(),
            None,
            H160([1; 20]),
            H160([1; 20]),
        );

        let query = Query {
            kind: OrderKind::Sell,
            ..Default::default()
        };
        let result = futures::try_join!(trader.get_quote(&query), trader.get_trade(&query));

        assert!(result.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn real_estimate() {
        let weth = testlib::tokens::WETH;
        let gno = testlib::tokens::GNO;

        let one_inch =
            OneInchClientImpl::new(OneInchClientImpl::DEFAULT_URL, Client::new(), 1).unwrap();
        let estimator = create_trade_finder(one_inch);

        let result = estimator
            .get_trade(&Query {
                verification: None,
                sell_token: weth,
                buy_token: gno,
                in_amount: NonZeroU256::try_from(10u128.pow(18)).unwrap(),
                kind: OrderKind::Sell,
            })
            .await;

        let trade = result.unwrap();
        println!(
            "1 WETH buys {} GNO, costing {} gas",
            trade.out_amount.to_f64_lossy() / 1e18,
            trade.gas_estimate,
        );
    }

    #[tokio::test]
    async fn spender_gets_cached() {
        const MAX_AGE: Duration = Duration::from_millis(10);
        let spender = |address: u64| Spender {
            address: H160::from_low_u64_be(address),
        };
        let mock_api = |address: u64| {
            let mut one_inch = MockOneInchClient::new();
            one_inch
                .expect_get_spender()
                .returning(move || async move { Ok(spender(address)) }.boxed())
                .times(1);
            one_inch
        };

        let mut inner = Inner {
            cache: Cache::new(MAX_AGE),
            ..Inner::new(
                Arc::new(mock_api(1)),
                vec![],
                None,
                H160([1; 20]),
                H160([1; 20]),
            )
        };

        // Calling `Inner::spender()` twice within `MAX_AGE` will return
        // the same result twice and only issue one call to `OneInchClient::spender()`.
        let result = inner.spender().await.unwrap();
        assert_eq!(result, spender(1).address);
        let result = inner.spender().await.unwrap();
        assert_eq!(result, spender(1).address);

        // Use a different mock instance to allow returning a new value from the
        // `spender()` function.
        inner.api = Arc::new(mock_api(2));

        // After `MAX_AGE` calling `Inner::spender()` again will result in
        // another call to `OneInchClient::spender()` because the cached value
        // expired.
        tokio::time::sleep(MAX_AGE).await;
        let result = inner.spender().await.unwrap();
        assert_eq!(result, spender(2).address);
    }
}
