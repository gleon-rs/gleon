//! Data parallelism for the comparison passes: rayon with the `parallel` feature (the CLI, which
//! compares one golden at a time on every core), plain iterators without it (`gleon-ffi`: test
//! runners parallelize across worker processes, so a pool per process would only add threads and
//! a cross-thread hand-off per pass).
//!
//! Both variants visit the same chunks and combine the same values; only the order of a float
//! reduction may differ (as it does between thread counts).

/// Maps every `size`-element chunk of `data` (with its index) and reduces the results.
pub fn map_chunks_mut<T, R>(
    data: &mut [T],
    size: usize,
    map: impl Fn(usize, &mut [T]) -> R + Sync + Send,
    identity: impl Fn() -> R + Sync + Send,
    reduce: impl Fn(R, R) -> R + Sync + Send,
) -> R
where
    T: Send,
    R: Send,
{
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        data.par_chunks_mut(size)
            .enumerate()
            .map(|(index, chunk)| map(index, chunk))
            .reduce(identity, reduce)
    }
    #[cfg(not(feature = "parallel"))]
    {
        data.chunks_mut(size)
            .enumerate()
            .map(|(index, chunk)| map(index, chunk))
            .fold(identity(), reduce)
    }
}

/// Runs `visit` on every `size`-element chunk of `data` with its index.
pub fn for_each_chunk_mut<T: Send>(
    data: &mut [T],
    size: usize,
    visit: impl Fn(usize, &mut [T]) + Sync + Send,
) {
    map_chunks_mut(data, size, visit, || (), |(), ()| ());
}
