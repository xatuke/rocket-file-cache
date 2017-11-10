# Rocket File Cache
An in-memory file cache for the Rocket web framework.

Rocket File Cache can be used as a drop in replacement for Rocket's NamedFile when serving files.

This:
```rust
#[get("/<file..>")]
fn files(file: PathBuf) -> Option<NamedFile> {
    NamedFile::open(Path::new("static/").join(file)).ok()
}
```
Can be replaced with:
```rust
fn main() {
    let cache: Mutex<Cache> = Mutex::new(Cache::new(1024 * 1024 * 10)); // 10 megabytes
    rocket::ignite()
        .manage(cache)
        .mount("/", routes![files])
        .launch();
}

#[get("/<file..>")]
fn files(file: PathBuf, cache: State<Mutex<Cache>>) -> Option<CachedFile> {
    let pathbuf: PathBuf = Path::new("www/").join(file).to_owned();
    cache.lock().unwrap().get_or_cache(pathbuf)
}
```


# Should I use this?
Rocket File Cache keeps a set of frequently accessed files in memory so your webserver won't have to wait for your disk to read the files.
This should improve latency and throughput on systems that are bottlenecked on disk I/O.

Because the cache needs to be hidden behind a Mutex, only one thread can get access at a time.
This will have a negative performance impact in cases where the webserver is handling enough traffic to constantly cause lock contention.

# Performance

The bench tests try to get the file from whatever source, and read it once into memory.
The misses measure the time it takes for the cache to realize that the file is not stored, and to read the file from disk.
Running the unscientific bench tests on an AWS EC2 t2 micro instance (82 MB/s HDD) returned these results:
```
test cache::tests::cache_get_10mb                       ... bench:   3,762,150 ns/iter (+/- 330,904)
test cache::tests::cache_get_1mb                        ... bench:      79,753 ns/iter (+/- 1,363)
test cache::tests::cache_get_1mb_from_1000_entry_cache  ... bench:      78,280 ns/iter (+/- 976)
test cache::tests::cache_get_5mb                        ... bench:   1,724,274 ns/iter (+/- 51,107)
test cache::tests::cache_miss_10mb                      ... bench:  14,115,702 ns/iter (+/- 885,753)
test cache::tests::cache_miss_1mb                       ... bench:     893,150 ns/iter (+/- 22,144)
test cache::tests::cache_miss_1mb_from_1000_entry_cache ... bench:   1,475,091 ns/iter (+/- 19,818)
test cache::tests::cache_miss_5mb                       ... bench:   4,931,435 ns/iter (+/- 384,645)
test cache::tests::cache_miss_5mb_from_1000_entry_cache ... bench:   6,500,311 ns/iter (+/- 272,567)
test cache::tests::named_file_read_10mb                 ... bench:   4,077,586 ns/iter (+/- 671,434)
test cache::tests::named_file_read_1mb                  ... bench:   1,043,831 ns/iter (+/- 23,470)
test cache::tests::named_file_read_5mb                  ... bench:   2,388,722 ns/iter (+/- 80,227)
```

It can be seen that on a server with slow disk reads, small file access times are vastly improved versus the disk.

That said, because the cache is guarded by a Mutex, synchronous access is impeded, possibly slowing down the effective serving rate of the webserver.

This performance hit can be mitigated by using a pool of caches at the expense of increased memory use,
or by immediately falling back to getting files from the filesystem if a lock can't be gained.


I have seen significant speedups for servers that serve small files that are only sporadically accessed.
I cannot recommend the use of this library outside of that use case until further benchmarks are performed.
Cache misses impact performance heavily, so setting a maximum file size for cache eligibility is suggested - likely somewhere around 3MB currently.

# Warning
This crate is still under development.

Worst case performance is being worked on.
Currently, the worst case performance is exhibited when a cache with many entries tries to add another file that
requires invalidation of every existing entry to fit; then it finds that the new file's priority is not higher than
the files it needs to remove, so it needs to read from disk anyway.

It is understandable that the performance decreases with every additional entry. but what is difficult to explain, is
 that this performance is affected by the size of the files already in the cache.

Ideally, a solution can be found that will make worst case performance dependent only on the number of files in the cache,
not the size of those files.


### Things that may change:
* Default priority function. Because performance seems to be most improved for smaller files, the default priority function
may change to favor smaller files in the future.
* Cache-invalidation is slower than expected, and is dependent on the size of the files in the cache.
This hopefully should change.

# Documentation
Documentation can be found here: https://docs.rs/crate/rocket-file-cache
