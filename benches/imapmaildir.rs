//! Benchmarks for regression detection, see `scripts/bench-compare.sh`.
//!
//! Bench ids are compared against baselines of other commits. Renaming a bench drops its history.
//! Group prefixes `state/`, `repo/` and `pipeline/` are classified as io benches by
//! `scripts/check-bench-regression.sh`, everything else as cpu benches.

use std::{
    hint::black_box,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use criterion::{BatchSize, Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use imapmaildir::bench::{
    Entry, Mail, Repo, StateDb, build_sequence_set, decode_all, format_maildir_filename,
    parse_maildir_filename, runtime_info,
};
use tempfile::TempDir;

const FILEPREFIX: &str = "1760000000.P12345N123456789.hostname";
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

fn cpu(c: &mut Criterion) {
    let mut group = c.benchmark_group("codec");
    let mut fetch_batch = Vec::new();
    for i in 1..=1000 {
        fetch_batch.extend_from_slice(
            format!("* {i} FETCH (UID {i} FLAGS (\\Seen \\Answered) MODSEQ ({i}))\r\n").as_bytes(),
        );
    }
    group.throughput(Throughput::Bytes(fetch_batch.len() as u64));
    group.bench_function("fetch_batch", |b| {
        b.iter_batched(
            || BytesMut::from(fetch_batch.as_slice()),
            |buf| assert_eq!(decode_all(buf), 1000),
            BatchSize::SmallInput,
        );
    });

    let content = mail_content(100 * 1024);
    let mut body = format!(
        "* 1 FETCH (UID 1 MODSEQ (5) FLAGS (\\Seen) BODY[] {{{}}}\r\n",
        content.len()
    )
    .into_bytes();
    body.extend_from_slice(&content);
    body.extend_from_slice(b")\r\n");
    group.throughput(Throughput::Bytes(body.len() as u64));
    group.bench_function("body_100KiB", |b| {
        b.iter_batched(
            || BytesMut::from(body.as_slice()),
            |buf| assert_eq!(decode_all(buf), 1),
            BatchSize::SmallInput,
        );
    });
    group.finish();

    let mut group = c.benchmark_group("maildir");
    let tracked = format!("{FILEPREFIX},U=4711:2,FS");
    let new = format!("{FILEPREFIX}:2,S");
    group.bench_function("parse_filename", |b| {
        b.iter(|| {
            (
                parse_maildir_filename(black_box(&tracked)),
                parse_maildir_filename(black_box(&new)),
            )
        });
    });
    group.bench_function("format_filename", |b| {
        b.iter_batched(
            || FILEPREFIX.to_string(),
            |prefix| format_maildir_filename(black_box(4711), black_box("FS"), prefix),
            BatchSize::SmallInput,
        );
    });
    group.finish();

    let mut group = c.benchmark_group("sequence_set");
    let contiguous: Vec<u32> = (1..=10_000).collect();
    let sparse: Vec<u32> = (1..=10_000).filter(|uid| uid % 3 != 0).collect();
    group.bench_function("build_contiguous_10k", |b| {
        b.iter(|| build_sequence_set(black_box(&contiguous)));
    });
    group.bench_function("build_sparse_10k", |b| {
        b.iter(|| build_sequence_set(black_box(&sparse)));
    });
    group.finish();
}

fn state(c: &mut Criterion) {
    let mut group = c.benchmark_group("state");
    group
        .sample_size(20)
        .sampling_mode(SamplingMode::Flat)
        .throughput(Throughput::Elements(u64::from(IO_MAILS)));
    let entries = |flags: &str| -> Vec<Entry> {
        (1..=IO_MAILS)
            .map(|uid| Entry::new(uid, flags, format!("{FILEPREFIX}{uid}")))
            .collect()
    };
    group.bench_function("store", |b| {
        b.iter_batched(
            || {
                let dir = temp_dir();
                let db = StateDb::init(dir.path());
                (dir, db, entries("S"))
            },
            |(dir, db, entries)| {
                db.store_all(&entries);
                (dir, db)
            },
            BatchSize::PerIteration,
        );
    });
    let dir = temp_dir();
    let db = StateDb::init(dir.path());
    db.store_all(&entries("S"));
    let toggled = [entries("FS"), entries("S")];
    let iteration = AtomicU32::new(0);
    group.bench_function("update", |b| {
        b.iter(|| {
            let i = iteration.fetch_add(1, Ordering::Relaxed) as usize % 2;
            db.update_all(&toggled[i]);
        });
    });
    group.bench_function("for_each", |b| {
        b.iter(|| assert_eq!(db.count(), IO_MAILS as usize));
    });
    drop(db);
    drop(dir);
    group.finish();
}

fn repo(c: &mut Criterion) {
    println!("runtime: {}", runtime_info());

    for (name, size) in [("4KiB", 4 * 1024), ("100KiB", 100 * 1024)] {
        let mut group = c.benchmark_group("repo");
        group
            .sample_size(20)
            .sampling_mode(SamplingMode::Flat)
            .measurement_time(SLOW_MEASUREMENT_TIME)
            .throughput(Throughput::Elements(u64::from(IO_MAILS)));
        let content = mail_content(size);
        group.bench_function(format!("store_{name}"), |b| {
            b.iter_batched(
                || (init_repo(), mails(1..=IO_MAILS, &content)),
                |((dir, repo), mails)| {
                    repo.store_all(mails);
                    (dir, repo)
                },
                BatchSize::PerIteration,
            );
        });
        group.finish();
    }

    let mut group = c.benchmark_group("repo");
    group
        .sample_size(20)
        .sampling_mode(SamplingMode::Flat)
        .measurement_time(SLOW_MEASUREMENT_TIME)
        .throughput(Throughput::Elements(u64::from(IO_MAILS)));
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
    targets = cpu, state, repo, pipeline
}
criterion_main!(benches);
