mod fetch;

pub use fetch::spawn_fetch;

/// One generation of fetching. `App` owns the counter and hands out ids; the
/// event loop drops any net message whose id isn't the current generation, so
/// a slow stale fetch can never clobber a newer one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FetchId(pub u64);
