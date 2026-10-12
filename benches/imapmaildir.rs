//! Benchmarks of code paths affected by the async runtime, see `scripts/bench-compare.sh`.
//!
//! Bench ids are compared against baselines of other commits. Renaming a bench drops its history.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use bytes::Bytes;
use criterion::{BatchSize, Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use imapmaildir::bench::{Mail, Repo, runtime_info};
use tempfile::TempDir;

const IO_MAILS: u32 = 100;
const DETECT_MAILS: u32 = 5000;
const SLOW_MEASUREMENT_TIME: Duration = Duration::from_secs(20);

fn temp_dir() -> TempDir {
    let dir = std::env::var_os("BENCH_TMPDIR").map_or_else(
        || {
            let exe = std::env::current_exe().expect("bench executable path should be known");
            exe.ancestors()
                .nth(3)
                .expect("bench executable should be in target dir")
                .join("bench-tmp")
        },
        PathBuf::from,
    );
    std::fs::create_dir_all(&dir).expect("bench temp dir should be creatable");
    tempfile::Builder::new()
        .prefix("bench")
        .tempdir_in(dir)
        .expect("temp dir should be creatable")
}

fn mail_content(size: usize) -> Bytes {
    let line = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod.\r\n";
    let mut content =
        b"From: a@example.com\r\nTo: b@example.com\r\nSubject: bench\r\n\r\n".to_vec();
    while content.len() < size {
        content.extend_from_slice(line);
    }
    content.truncate(size);

    content.into()
}

fn mails(uids: impl Iterator<Item = u32>, content: &Bytes) -> Vec<Mail> {
    uids.map(|uid| Mail::new(uid, "S", i64::from(uid) + 1, content.clone()))
        .collect()
}

fn init_repo() -> (TempDir, Repo) {
    let dir = temp_dir();
    let repo = Repo::init(&dir.path().join("mail"), &dir.path().join("state"));

    (dir, repo)
}

fn init_repo_with_mails(count: u32) -> (TempDir, Repo) {
    let (dir, repo) = init_repo();
    repo.store_all(mails(1..=count, &mail_content(4 * 1024)));

    (dir, repo)
}

fn repo(c: &mut Criterion) {
    println!("runtime: {}", runtime_info());

    let mut group = c.benchmark_group("repo");
    group
        .sample_size(20)
        .sampling_mode(SamplingMode::Flat)
        .measurement_time(SLOW_MEASUREMENT_TIME)
        .throughput(Throughput::Elements(u64::from(IO_MAILS)));
    let content = mail_content(4 * 1024);
    group.bench_function("store_4KiB", |b| {
        b.iter_batched(
            || (init_repo(), mails(1..=IO_MAILS, &content)),
            |((dir, repo), mails)| {
                repo.store_all(mails);
                (dir, repo)
            },
            BatchSize::PerIteration,
        );
    });

    let uids: Vec<u32> = (1..=IO_MAILS).collect();
    let (dir, repo) = init_repo_with_mails(IO_MAILS);
    let modseq = AtomicU32::new(IO_MAILS + 2);
    group.bench_function("update_flags", |b| {
        b.iter(|| {
            let modseq = modseq.fetch_add(1, Ordering::Relaxed);
            let flags = if modseq.is_multiple_of(2) { "FS" } else { "S" };
            repo.update_flags_all(&uids, flags, i64::from(modseq));
        });
    });
    drop(repo);
    drop(dir);
    group.bench_function("delete", |b| {
        b.iter_batched(
            || init_repo_with_mails(IO_MAILS),
            |(dir, repo)| {
                repo.delete_all(&uids);
                (dir, repo)
            },
            BatchSize::PerIteration,
        );
    });
    group.throughput(Throughput::Elements(u64::from(DETECT_MAILS)));
    let (dir, repo) = init_repo_with_mails(DETECT_MAILS);
    group.bench_function("detect_changes", |b| {
        b.iter(|| assert_eq!(repo.detect_changes(), 0));
    });
    drop(repo);
    drop(dir);
    group.finish();
}

fn pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline");
    group
        .sample_size(20)
        .sampling_mode(SamplingMode::Flat)
        .measurement_time(SLOW_MEASUREMENT_TIME)
        .throughput(Throughput::Elements(u64::from(IO_MAILS)));
    let content = mail_content(4 * 1024);
    group.bench_function("new_mail", |b| {
        b.iter_batched(
            || (init_repo(), mails(1..=IO_MAILS, &content)),
            |((dir, repo), mails)| {
                repo.process_new_mails(mails);
                (dir, repo)
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn config() -> Criterion {
    Criterion::default()
        .confidence_level(0.99)
        .measurement_time(Duration::from_secs(5))
        .configure_from_args()
}

criterion_group! {
    name = benches;
    config = config();
    targets = repo, pipeline
}
criterion_main!(benches);
