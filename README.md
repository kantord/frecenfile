# frecenfile

**frecenfile** computes _frecency_ scores for files in Git repositories. Frecency combines the frequency and recency of
events.

This is useful as a heauristic for finding relevant or trending files when all you have to work with is the
commit history.

## Performance
**frecenfile** is highly scalabe, producing a sorted output within miliseconds for mid-sized repositories, and
processing the entire commit history Linux in under a minute. Processing the last 3000 commits in 
the Linux repository takes just around a second.

For most purposes, the results should be easily cacheable.

## Git history

By default, **frecenfile** processes the last 3000 commits, but this can be modified using the `--max-commits`
flag. Processing an excessive amounts of commits would not usually be usueful, as "trending" files
are not likely to be buried deep in the commit history. Processing only a smaller amount of commits is not
likely to be needed for performance reasons, but might be useful for some use cases.

## 📦 Installation

```bash
cargo install frecenfile
```

## 🚀 Usage

### Score every file in the current repo, highest first

```bash
frecenfile
```

### Only list paths, omit scores

```bash
frecenfile --path-only
```

### Restrict analysis to certain directories

```bash
frecenfile --paths src tests
```

### Sort oldest/least-touched files first

```bash
frecenfile --ascending
```

### Example output

```
12.9423   src/lib.rs
 9.3310   src/analyze.rs
 2.7815   README.md
```
