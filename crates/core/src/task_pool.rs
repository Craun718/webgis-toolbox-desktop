//! 有界批处理，结果按输入顺序交还给任务协调线程。

use crate::i18n::{language, with_language};
use rayon::prelude::*;
pub use rayon::{ThreadPool, ThreadPoolBuilder};

pub fn width(pool: Option<&ThreadPool>) -> usize {
    pool.map_or(1, ThreadPool::current_num_threads)
}

pub fn batches<I: Iterator>(
    mut input: I,
    pool: Option<&ThreadPool>,
) -> impl Iterator<Item = Vec<I::Item>> {
    let width = width(pool);
    std::iter::from_fn(move || {
        let batch: Vec<_> = input.by_ref().take(width).collect();
        (!batch.is_empty()).then_some(batch)
    })
}

pub fn map<T: Send, R: Send, E: Send>(
    pool: Option<&ThreadPool>,
    batch: Vec<T>,
    work: impl Fn(T) -> Result<R, E> + Sync,
) -> Result<Vec<R>, E> {
    match pool {
        Some(pool) if batch.len() > 1 => {
            let language = language();
            let results: Vec<_> = pool.install(|| {
                batch
                    .into_par_iter()
                    .map(|item| with_language(language, || work(item)))
                    .collect()
            });
            results.into_iter().collect()
        }
        _ => batch.into_iter().map(work).collect(),
    }
}

pub fn map_with<T: Send, S: Send, R: Send, E: Send>(
    pool: Option<&ThreadPool>,
    batch: Vec<T>,
    states: &mut [S],
    work: impl Fn(T, &mut S) -> Result<R, E> + Sync,
) -> Result<Vec<R>, E> {
    let states = &mut states[..batch.len()];
    match pool {
        Some(pool) if batch.len() > 1 => {
            let language = language();
            let results: Vec<_> = pool.install(|| {
                batch
                    .into_par_iter()
                    .zip(states.par_iter_mut())
                    .map(|(item, state)| with_language(language, || work(item, state)))
                    .collect()
            });
            results.into_iter().collect()
        }
        _ => batch
            .into_iter()
            .zip(states)
            .map(|(item, state)| work(item, state))
            .collect(),
    }
}
