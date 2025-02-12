use {
    crate::{
        baseline_solver::{self, estimate_buy_amount, estimate_sell_amount, BaseTokens},
        conversions::U256Ext,
        price_estimation::{
            gas,
            Estimate,
            PriceEstimateResult,
            PriceEstimating,
            PriceEstimationError,
            Query,
        },
        recent_block_cache::Block,
        sources::uniswap_v2::pool_fetching::{Pool, PoolFetching},
    },
    anyhow::Result,
    ethcontract::{H160, U256},
    futures::FutureExt as _,
    gas_estimation::GasPriceEstimating,
    model::{order::OrderKind, TokenPair},
    num::BigRational,
    number::nonzero::U256 as NonZeroU256,
    std::{collections::HashMap, sync::Arc},
};

pub struct BaselinePriceEstimator {
    pool_fetcher: Arc<dyn PoolFetching>,
    gas_estimator: Arc<dyn GasPriceEstimating>,
    base_tokens: Arc<BaseTokens>,
    native_token: H160,
    native_token_price_estimation_amount: NonZeroU256,
    solver: H160,
}

impl BaselinePriceEstimator {
    pub fn new(
        pool_fetcher: Arc<dyn PoolFetching>,
        gas_estimator: Arc<dyn GasPriceEstimating>,
        base_tokens: Arc<BaseTokens>,
        native_token: H160,
        native_token_price_estimation_amount: NonZeroU256,
        solver: H160,
    ) -> Self {
        Self {
            pool_fetcher,
            gas_estimator,
            base_tokens,
            native_token,
            native_token_price_estimation_amount,
            solver,
        }
    }
}

type Pools = HashMap<TokenPair, Vec<Pool>>;

impl PriceEstimating for BaselinePriceEstimator {
    fn estimate(&self, query: Arc<Query>) -> futures::future::BoxFuture<'_, PriceEstimateResult> {
        async move {
            let gas_price = async {
                let gas_price = self
                    .gas_estimator
                    .estimate()
                    .await
                    .map_err(PriceEstimationError::ProtocolInternal)?;
                Ok(gas_price.effective_gas_price())
            };
            let pools = async {
                self.pools_for_query(&query)
                    .await
                    .map_err(PriceEstimationError::ProtocolInternal)
            };

            let (gas_price, pools) = futures::future::try_join(gas_price, pools).await?;
            let (path, out_amount) = self.estimate_price_helper(&query, true, &pools, gas_price)?;
            let gas = estimate_gas(path.len());
            Ok(Estimate {
                out_amount,
                gas,
                solver: self.solver,
            })
        }
        .boxed()
    }
}

impl BaselinePriceEstimator {
    async fn pools_for_query(&self, query: &Query) -> Result<Pools> {
        let pairs = self
            .base_tokens
            .relevant_pairs(TokenPair::new(query.buy_token, query.sell_token).into_iter());
        let pools = self.pool_fetcher.fetch(pairs, Block::Recent).await?;
        Ok(pools_vec_to_map(pools))
    }

    /// Returns the path and the out amount.
    fn estimate_price_helper(
        &self,
        query: &Query,
        consider_gas_costs: bool,
        pools: &Pools,
        gas_price: f64,
    ) -> Result<(Vec<H160>, U256), PriceEstimationError> {
        if query.sell_token == query.buy_token {
            return Ok((Vec::new(), query.in_amount.get()));
        }
        match query.kind {
            OrderKind::Buy => {
                // Do not consider gas costs below to avoid infinite recursion.
                let sell_token_price_in_native_token = if consider_gas_costs {
                    Some(if query.sell_token == self.native_token {
                        num::one()
                    } else {
                        let buy_amount = self
                            .best_execution_sell_order(
                                self.native_token,
                                query.sell_token,
                                self.native_token_price_estimation_amount,
                                gas_price,
                                None,
                                pools,
                            )?
                            .1;
                        super::amounts_to_price(
                            self.native_token_price_estimation_amount.get(),
                            buy_amount,
                        )
                        .ok_or(PriceEstimationError::NoLiquidity)?
                    })
                } else {
                    None
                };
                let (path, sell_amount) = self.best_execution_buy_order(
                    query.sell_token,
                    query.buy_token,
                    query.in_amount,
                    gas_price,
                    sell_token_price_in_native_token,
                    pools,
                )?;
                Ok((path, sell_amount))
            }
            OrderKind::Sell => {
                // Do not consider gas costs below to avoid infinite recursion.
                let buy_token_price_in_native_token = if consider_gas_costs {
                    Some(if query.buy_token == self.native_token {
                        num::one()
                    } else {
                        let buy_amount = self
                            .best_execution_sell_order(
                                self.native_token,
                                query.buy_token,
                                self.native_token_price_estimation_amount,
                                gas_price,
                                None,
                                pools,
                            )?
                            .1;
                        super::amounts_to_price(
                            self.native_token_price_estimation_amount.get(),
                            buy_amount,
                        )
                        .ok_or(PriceEstimationError::NoLiquidity)?
                    })
                } else {
                    None
                };
                let (path, buy_amount) = self.best_execution_sell_order(
                    query.sell_token,
                    query.buy_token,
                    query.in_amount,
                    gas_price,
                    buy_token_price_in_native_token,
                    pools,
                )?;
                Ok((path, buy_amount))
            }
        }
    }

    /// Returns path and out (buy) amount.
    /// If buy_token_price_in_native_token is set then it will be used to take
    /// gas cost into account.
    fn best_execution_sell_order(
        &self,
        sell_token: H160,
        buy_token: H160,
        sell_amount: NonZeroU256,
        gas_price: f64,
        buy_token_price_in_native_token: Option<BigRational>,
        pools: &Pools,
    ) -> Result<(Vec<H160>, U256), PriceEstimationError> {
        let path_comparison = |buy_estimate: baseline_solver::Estimate<U256, Pool>| {
            if let Some(buy_token_price_in_native_token) = &buy_token_price_in_native_token {
                let buy_amount_in_native_token =
                    buy_estimate.value.to_big_rational() * buy_token_price_in_native_token;
                let tx_cost_in_native_token = U256::from_f64_lossy(gas_price).to_big_rational()
                    * BigRational::from_integer(buy_estimate.gas_cost().into());
                buy_amount_in_native_token - tx_cost_in_native_token
            } else {
                buy_estimate.value.to_big_rational()
            }
        };

        let (path, buy_amount) = self.best_execution(
            sell_token,
            buy_token,
            sell_amount,
            |amount, path, pools| {
                estimate_buy_amount(amount, path, pools)
                    .map(path_comparison)
                    .unwrap_or_else(|| -U256::max_value().to_big_rational())
            },
            |amount, path, pools| {
                estimate_buy_amount(amount, path, pools).map(|estimate| estimate.value)
            },
            pools,
        )?;
        Ok((path, buy_amount))
    }

    /// Returns path and out (sell) amount.
    /// If sell_token_price_in_native_token is set then it will be used to take
    /// gas cost into account.
    fn best_execution_buy_order(
        &self,
        sell_token: H160,
        buy_token: H160,
        buy_amount: NonZeroU256,
        gas_price: f64,
        sell_token_price_in_native_token: Option<BigRational>,
        pools: &Pools,
    ) -> Result<(Vec<H160>, U256), PriceEstimationError> {
        let path_comparison = |sell_estimate: baseline_solver::Estimate<U256, Pool>| {
            if let Some(sell_token_price_in_native_token) = &sell_token_price_in_native_token {
                let sell_amount_in_native_token =
                    sell_estimate.value.to_big_rational() * sell_token_price_in_native_token;
                let tx_cost_in_native_token = U256::from_f64_lossy(gas_price).to_big_rational()
                    * BigRational::from_integer(sell_estimate.gas_cost().into());
                -sell_amount_in_native_token - tx_cost_in_native_token
            } else {
                -sell_estimate.value.to_big_rational()
            }
        };

        let (path, sell_amount) = self.best_execution(
            sell_token,
            buy_token,
            buy_amount,
            |amount, path, pools| {
                estimate_sell_amount(amount, path, pools)
                    .map(path_comparison)
                    .unwrap_or_else(|| -U256::max_value().to_big_rational())
            },
            |amount, path, pools| {
                estimate_sell_amount(amount, path, pools).map(|estimate| estimate.value)
            },
            pools,
        )?;
        Ok((path, sell_amount))
    }

    fn best_execution<AmountFn, CompareFn, O, Amount>(
        &self,
        sell_token: H160,
        buy_token: H160,
        amount: NonZeroU256,
        comparison: CompareFn,
        resulting_amount: AmountFn,
        pools: &Pools,
    ) -> Result<(Vec<H160>, Amount), PriceEstimationError>
    where
        AmountFn: Fn(U256, &[H160], &HashMap<TokenPair, Vec<Pool>>) -> Option<Amount>,
        CompareFn: Fn(U256, &[H160], &HashMap<TokenPair, Vec<Pool>>) -> O,
        O: Ord,
    {
        debug_assert!(sell_token != buy_token);

        let path_candidates = self.base_tokens.path_candidates(sell_token, buy_token);
        let best_path = path_candidates
            .iter()
            .max_by_key(|path| comparison(amount.get(), path, pools))
            .ok_or(PriceEstimationError::NoLiquidity)?;
        let resulting_amount = resulting_amount(amount.get(), best_path, pools)
            .ok_or(PriceEstimationError::NoLiquidity)?;
        Ok((best_path.clone(), resulting_amount))
    }
}

fn pools_vec_to_map(pools: Vec<Pool>) -> Pools {
    pools.into_iter().fold(Pools::new(), |mut pools, pool| {
        pools.entry(pool.tokens).or_default().push(pool);
        pools
    })
}

fn estimate_gas(path_len: usize) -> u64 {
    let hops = match path_len.checked_sub(1) {
        Some(len) => len,
        None => return 0,
    };
    // Can be reduced to one erc20 transfer when #675 is fixed.
    let per_hop = gas::ERC20_TRANSFER * 2 + 40_000;
    gas::SETTLEMENT_SINGLE_TRADE + per_hop * (hops as u64)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            baseline_solver::BaselineSolvable,
            gas_price_estimation::FakeGasPriceEstimator,
            sources::uniswap_v2::pool_fetching::{test_util::FakePoolFetcher, Pool},
        },
        gas_estimation::gas_price::GasPrice1559,
        std::sync::Mutex,
    };

    #[tokio::test]
    async fn return_error_if_no_token_found() {
        let token_a = H160::from_low_u64_be(1);
        let token_b = H160::from_low_u64_be(2);
        let pool_fetcher = Arc::new(FakePoolFetcher(vec![]));
        let gas_estimator = Arc::new(FakeGasPriceEstimator(Arc::new(Mutex::new(
            Default::default(),
        ))));
        let base_tokens = Arc::new(BaseTokens::new(H160::zero(), &[]));
        let estimator = BaselinePriceEstimator::new(
            pool_fetcher,
            gas_estimator,
            base_tokens,
            token_a,
            NonZeroU256::try_from(1).unwrap(),
            H160([1; 20]),
        );

        assert!(estimator
            .estimate(Arc::new(Query {
                verification: None,
                sell_token: token_a,
                buy_token: token_b,
                in_amount: NonZeroU256::try_from(1).unwrap(),
                kind: OrderKind::Buy
            }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn return_error_if_invalid_reserves() {
        let token_a = H160::from_low_u64_be(1);
        let token_b = H160::from_low_u64_be(2);
        let pool_address = H160::from_low_u64_be(1);
        let pool = Pool::uniswap(
            pool_address,
            TokenPair::new(token_a, token_b).unwrap(),
            (0, 10),
        );

        let pool_fetcher = Arc::new(FakePoolFetcher(vec![pool]));
        let gas_estimator = Arc::new(FakeGasPriceEstimator(Arc::new(Mutex::new(
            Default::default(),
        ))));
        let base_tokens = Arc::new(BaseTokens::new(H160::zero(), &[]));
        let estimator = BaselinePriceEstimator::new(
            pool_fetcher,
            gas_estimator,
            base_tokens,
            token_a,
            NonZeroU256::try_from(1).unwrap(),
            H160([1; 20]),
        );

        assert!(estimator
            .estimate(Arc::new(Query {
                verification: None,
                sell_token: token_a,
                buy_token: token_b,
                in_amount: NonZeroU256::try_from(1).unwrap(),
                kind: OrderKind::Buy
            }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn price_estimate_containing_valid_and_invalid_paths() {
        let token_a = H160::from_low_u64_be(1);
        let token_b = H160::from_low_u64_be(2);
        let address = H160::from_low_u64_be(1);

        // The path via the base token does not exist (making it an invalid path)
        let base_token = H160::from_low_u64_be(3);

        let pool = Pool::uniswap(
            address,
            TokenPair::new(token_a, token_b).unwrap(),
            (10u128.pow(28), 10u128.pow(27)),
        );

        let pool_fetcher = Arc::new(FakePoolFetcher(vec![pool]));
        let gas_estimator = Arc::new(FakeGasPriceEstimator(Arc::new(Mutex::new(
            Default::default(),
        ))));
        let base_tokens = Arc::new(BaseTokens::new(base_token, &[]));
        let estimator = BaselinePriceEstimator::new(
            pool_fetcher,
            gas_estimator,
            base_tokens,
            token_b,
            NonZeroU256::try_from(1).unwrap(),
            H160([1; 20]),
        );

        assert!(estimator
            .estimate(Arc::new(Query {
                verification: None,
                sell_token: token_a,
                buy_token: token_b,
                in_amount: NonZeroU256::try_from(100).unwrap(),
                kind: OrderKind::Sell
            }))
            .await
            .is_ok());
        assert!(estimator
            .estimate(Arc::new(Query {
                verification: None,
                sell_token: token_a,
                buy_token: token_b,
                in_amount: NonZeroU256::try_from(100).unwrap(),
                kind: OrderKind::Buy
            }))
            .await
            .is_ok());
    }

    fn pool_price(
        pool: &Pool,
        token_out: H160,
        amount_in: impl Into<U256>,
        token_in: H160,
    ) -> BigRational {
        let amount_in = amount_in.into();
        BigRational::new(
            amount_in.to_big_int(),
            pool.get_amount_out(token_out, (amount_in, token_in))
                .unwrap()
                .as_u128()
                .into(),
        )
    }

    #[tokio::test]
    async fn price_estimate_uses_best_pool() {
        let token_a = H160([0x0a; 20]);
        let token_b = H160([0x0b; 20]);

        let pools = vec![
            Pool::uniswap(
                H160::from_low_u64_be(1),
                TokenPair::new(token_a, token_b).unwrap(),
                (100_000, 100_000),
            ),
            Pool::uniswap(
                H160::from_low_u64_be(2),
                TokenPair::new(token_a, token_b).unwrap(),
                (100_000, 90_000),
            ),
        ];

        let pool_fetcher = Arc::new(FakePoolFetcher(pools.clone()));
        let gas_estimator = Arc::new(FakeGasPriceEstimator(Arc::new(Mutex::new(
            Default::default(),
        ))));
        let base_tokens = Arc::new(BaseTokens::new(H160::zero(), &[]));
        let estimator = BaselinePriceEstimator::new(
            pool_fetcher,
            gas_estimator,
            base_tokens,
            token_a,
            NonZeroU256::try_from(10).unwrap(),
            H160([1; 20]),
        );

        let query = Arc::new(Query {
            verification: None,
            sell_token: token_a,
            buy_token: token_b,
            in_amount: NonZeroU256::try_from(100).unwrap(),
            kind: OrderKind::Sell,
        });
        let estimate = estimator.estimate(query.clone()).await.unwrap();
        // Pool 0 is more favourable for buying token B.
        assert_eq!(
            estimate.price_in_sell_token_rational(&query).unwrap(),
            pool_price(&pools[0], token_b, 100, token_a)
        );

        let query = Arc::new(Query {
            verification: None,
            sell_token: token_b,
            buy_token: token_a,
            in_amount: NonZeroU256::try_from(100).unwrap(),
            kind: OrderKind::Sell,
        });
        let estimate = estimator.estimate(query.clone()).await.unwrap();
        // Pool 1 is more favourable for buying token A.
        assert_eq!(
            estimate.price_in_sell_token_rational(&query).unwrap(),
            pool_price(&pools[1], token_a, 100, token_b)
        );
    }

    #[tokio::test]
    async fn gas_estimate_returns_cost_of_best_path() {
        let token_a = H160::from_low_u64_be(1);
        let intermediate = H160::from_low_u64_be(2);
        let token_b = H160::from_low_u64_be(3);

        // Direct trade is better when selling token_b
        let pools = vec![
            Pool::uniswap(
                H160::from_low_u64_be(1),
                TokenPair::new(token_a, token_b).unwrap(),
                (1000, 1000),
            ),
            Pool::uniswap(
                H160::from_low_u64_be(2),
                TokenPair::new(token_a, intermediate).unwrap(),
                (900, 1000),
            ),
            Pool::uniswap(
                H160::from_low_u64_be(3),
                TokenPair::new(intermediate, token_b).unwrap(),
                (900, 1000),
            ),
        ];

        let pool_fetcher = Arc::new(FakePoolFetcher(pools));
        let gas_estimator = Arc::new(FakeGasPriceEstimator(Arc::new(Mutex::new(
            Default::default(),
        ))));
        let base_tokens = Arc::new(BaseTokens::new(intermediate, &[]));
        let estimator = BaselinePriceEstimator::new(
            pool_fetcher,
            gas_estimator,
            base_tokens,
            intermediate,
            NonZeroU256::try_from(10).unwrap(),
            H160([1; 20]),
        );

        for kind in &[OrderKind::Sell, OrderKind::Buy] {
            let intermediate = estimator
                .estimate(Arc::new(Query {
                    verification: None,
                    sell_token: token_a,
                    buy_token: token_b,
                    in_amount: NonZeroU256::try_from(1).unwrap(),
                    kind: *kind,
                }))
                .await
                .unwrap()
                .gas;
            assert_eq!(intermediate, estimate_gas(3));
            let direct = estimator
                .estimate(Arc::new(Query {
                    verification: None,
                    sell_token: token_b,
                    buy_token: token_a,
                    in_amount: NonZeroU256::try_from(10).unwrap(),
                    kind: *kind,
                }))
                .await
                .unwrap()
                .gas;
            assert_eq!(direct, estimate_gas(2));
            assert!(direct < intermediate);
        }
    }

    #[tokio::test]
    async fn price_estimate_takes_gas_costs_into_account() {
        let native = H160::from_low_u64_be(0);
        let sell = H160::from_low_u64_be(1);
        let intermediate = H160::from_low_u64_be(2);
        let buy = H160::from_low_u64_be(3);

        let pools = vec![
            // Native token connection for tokens 1, 2. Note that the connection has a price much
            // worse than the pools between 1, 2, 3 so that it is not used for the trade, just for
            // gas price.
            Pool::uniswap(
                H160::from_low_u64_be(1),
                TokenPair::new(native, sell).unwrap(),
                (100_000_000_000, 2_000),
            ),
            Pool::uniswap(
                H160::from_low_u64_be(2),
                TokenPair::new(native, buy).unwrap(),
                (100_000_000_000, 1_000),
            ),
            // Direct connection 1 to 3.
            Pool::uniswap(
                H160::from_low_u64_be(3),
                TokenPair::new(sell, buy).unwrap(),
                (1000, 800),
            ),
            // Intermediate from 1 to 2 to 2, cheaper than direct.
            Pool::uniswap(
                H160::from_low_u64_be(4),
                TokenPair::new(sell, intermediate).unwrap(),
                (1000, 1000),
            ),
            Pool::uniswap(
                H160::from_low_u64_be(5),
                TokenPair::new(intermediate, buy).unwrap(),
                (1000, 1000),
            ),
        ];

        let pool_fetcher = Arc::new(FakePoolFetcher(pools.clone()));
        let gas_estimator = Arc::new(FakeGasPriceEstimator(Arc::new(Mutex::new(GasPrice1559 {
            base_fee_per_gas: 0.0,
            max_fee_per_gas: 10000.0,
            max_priority_fee_per_gas: 10000.0,
        }))));
        let base_tokens = Arc::new(BaseTokens::new(native, &[intermediate]));
        let estimator = BaselinePriceEstimator::new(
            pool_fetcher,
            gas_estimator.clone(),
            base_tokens,
            native,
            NonZeroU256::try_from(1_000_000_000).unwrap(),
            H160([1; 20]),
        );

        // Uses 1 hop because high gas price doesn't make the intermediate hop worth it.
        for order_kind in [OrderKind::Sell, OrderKind::Buy].iter() {
            assert_eq!(
                estimator
                    .estimate(Arc::new(Query {
                        verification: None,
                        sell_token: sell,
                        buy_token: buy,
                        in_amount: NonZeroU256::try_from(10).unwrap(),
                        kind: *order_kind
                    }))
                    .await
                    .unwrap()
                    .gas,
                estimate_gas(2),
            );
        }

        // Reduce gas price.
        *gas_estimator.0.lock().unwrap() = GasPrice1559 {
            base_fee_per_gas: 0.0,
            max_fee_per_gas: 1.0,
            max_priority_fee_per_gas: 1.0,
        };

        // Lower gas price does make the intermediate hop worth it.
        for order_kind in [OrderKind::Sell, OrderKind::Buy].iter() {
            assert_eq!(
                estimator
                    .estimate(Arc::new(Query {
                        verification: None,
                        sell_token: sell,
                        buy_token: buy,
                        in_amount: NonZeroU256::try_from(10).unwrap(),
                        kind: *order_kind
                    }))
                    .await
                    .unwrap()
                    .gas,
                estimate_gas(3)
            );
        }
    }

    #[tokio::test]
    async fn estimate_price_honours_parameter_consider_gas_costs() {
        let token_a = H160::from_low_u64_be(1);
        let token_b = H160::from_low_u64_be(2);
        let token_c = H160::from_low_u64_be(3);

        // A->B->C prices buy token to 1.006 but costs 2*G.
        // A->C prices buy token to 1.007 but costs G.

        let pool_ab = Pool::uniswap(
            H160::from_low_u64_be(1),
            TokenPair::new(token_a, token_b).unwrap(),
            (10u128.pow(28), 10u128.pow(28)),
        );
        let pool_bc = Pool::uniswap(
            H160::from_low_u64_be(2),
            TokenPair::new(token_b, token_c).unwrap(),
            (10u128.pow(28), 10u128.pow(28)),
        );
        let pool_ac = Pool::uniswap(
            H160::from_low_u64_be(3),
            TokenPair::new(token_a, token_c).unwrap(),
            (1004 * 10u128.pow(25), 10u128.pow(28)),
        );
        let pools = pools_vec_to_map(vec![pool_ab, pool_bc, pool_ac]);

        let base_tokens = Arc::new(BaseTokens::new(token_b, &[]));
        let estimator = BaselinePriceEstimator::new(
            Arc::new(FakePoolFetcher::default()),
            Arc::new(FakeGasPriceEstimator::default()),
            base_tokens,
            token_a,
            NonZeroU256::try_from(10u128.pow(18)).unwrap(),
            H160([1; 20]),
        );

        let gas_price = 1000000000000000.0;
        let query = Query {
            verification: None,
            sell_token: token_a,
            buy_token: token_c,
            in_amount: NonZeroU256::try_from(10u128.pow(19)).unwrap(),
            kind: OrderKind::Sell,
        };
        let out_amount_considering_gas_costs = estimator
            .estimate_price_helper(&query, true, &pools, gas_price)
            .unwrap()
            .1;
        let out_amount_disregarding_gas_costs = estimator
            .estimate_price_helper(&query, false, &pools, gas_price)
            .unwrap()
            .1;
        assert!(out_amount_considering_gas_costs != out_amount_disregarding_gas_costs);
        assert!(out_amount_considering_gas_costs.to_f64_lossy() <= 1.008e19);
        assert!(out_amount_disregarding_gas_costs.to_f64_lossy() <= 1.008e19);
    }
}
