use crate::domain::{auction, solution};

pub mod baseline;
pub mod dex;
pub mod legacy;
pub mod naive;

pub use self::{baseline::Baseline, dex::Dex, legacy::Legacy, naive::Naive};

pub enum Solver {
    Baseline(Baseline),
    Naive(Naive),
    Legacy(Legacy),
    Dex(Dex),
}

impl Solver {
    /// Solves a given auction and returns multiple solutions. We allow
    /// returning multiple solutions to later merge multiple non-overlapping
    /// solutions to get one big more gas efficient solution.
    pub async fn solve(&self, auction: auction::Auction) -> Vec<solution::Solution> {
        match self {
            Solver::Baseline(solver) => solver.solve(auction).await,
            Solver::Naive(solver) => solver.solve(auction).await,
            Solver::Legacy(solver) => solver.solve(auction).await,
            Solver::Dex(solver) => solver.solve(auction).await,
        }
    }
}
