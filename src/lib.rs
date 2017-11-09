//#![feature(plugin)]
//#![plugin(rocket_codegen)]
#![feature(test)]

extern crate rocket;


#[macro_use]
extern crate log;

use rocket::request::Request;
use rocket::response::{Response, Responder};
use rocket::http::{Status, ContentType};


use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::io::BufReader;
use std::io::Read;
use std::io;
use std::result;
use std::usize;
use std::fmt;
use std::sync::Arc;

/// The structure that represents a file in memory.
/// Keeps a copy of the size of the file.
#[derive(Clone)]
pub struct SizedFile {
    bytes: Vec<u8>,
    size: usize
}

impl fmt::Debug for SizedFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // The byte array shouldn't be visible in the log.
        write!(f, "SizedFile {{ bytes: ..., size: {} }}", self.size )
    }
}


impl SizedFile {

    /// Reads the file at the path into a SizedFile.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<SizedFile> {
        let file = File::open(path.as_ref())?;
        let mut reader = BufReader::new(file);
        let mut buffer: Vec<u8> = vec!();
        let size: usize = reader.read_to_end(&mut buffer)?;

        Ok(SizedFile {
            bytes: buffer,
            size
        })
    }
}


/// The structure that is returned when a request to the cache is made.
/// The CachedFile knows its path, so it can set the content type when it is serialized to a request.
#[derive(Debug, Clone)]
pub struct CachedFile {
    path: PathBuf,
    file: Arc<SizedFile>
}



/// Streams the named file to the client. Sets or overrides the Content-Type in
/// the response according to the file's extension if the extension is
/// recognized. See
/// [ContentType::from_extension](/rocket/http/struct.ContentType.html#method.from_extension)
/// for more information. If you would like to stream a file with a different
/// Content-Type than that implied by its extension, use a `File` directly.
///
/// Based on NamedFile from rocket::response::NamedFile
impl Responder<'static> for CachedFile {
    fn respond_to(self, _: &Request) -> result::Result<Response<'static>, Status> {
        let mut response = Response::new();
        if let Some(ext) = self.path.extension() {
            if let Some(ct) = ContentType::from_extension(&ext.to_string_lossy()) {
                response.set_header(ct);
            }
        }

        // Convert the SizedFile into a raw pointer so its data can be used to set the streamed body
        // without explicit ownership.
        // This prevents copying the file, leading to a significant speedup.
        let file: *const SizedFile = Arc::into_raw(self.file);
        unsafe {
            response.set_streamed_body((*file).bytes.as_slice());
            let _ = Arc::from_raw(file); // Prevent dangling pointer?
        }

        Ok(response)
    }
}

#[derive(Debug, PartialEq)]
pub enum CacheInvalidationError {
    NoMoreFilesToRemove,
    NewPriorityIsNotHighEnough
}

#[derive(Debug, PartialEq)]
pub enum CacheInvalidationSuccess {
    ReplacedFile,
    InsertedFileIntoAvailableSpace
}

/// The Cache holds a set number of files.
/// The Cache acts as a proxy to the filesystem.
/// When a request for a file is made, the Cache checks to see if it has a copy of the file.
/// If it does have a copy, it returns the copy.
/// If it doesn't have a copy, it reads the file from the FS and tries to cache it.
/// If there is room in the Cache, the cache will store the file, otherwise it will increment a count indicating the number of access attempts for the file.
/// If the number of access attempts for the file are higher than the least in demand file in the Cache, the cache will replace the low demand file with the high demand file.
#[derive(Debug)]
pub struct Cache {
    size_limit: usize, // The number of bytes the file_map should ever hold.
    priority_function: PriorityFunction, // The priority function that is used to determine which files should be in the cache.
    file_map: HashMap<PathBuf, Arc<SizedFile>>, // Holds the files that the cache is caching
    access_count_map: HashMap<PathBuf, usize> // Every file that is accessed will have the number of times it is accessed logged in this map.
}


impl Cache {

    //TODO, consider moving to the builder pattern if min and max file sizes are added as options.
    /// Creates a new Cache with the given size limit and the default priority function.
    pub fn new(size_limit: usize) -> Cache {
        Cache {
            size_limit,
            priority_function: Cache::DEFAULT_PRIORITY_FUNCTION,
            file_map: HashMap::new(),
            access_count_map: HashMap::new()
        }
    }

    /// Creates a new Cache with the given size limit and a specified priority function.
    pub fn new_with_priority_function(size_limit: usize, priority_function: PriorityFunction) -> Cache {
        Cache {
            size_limit,
            priority_function,
            file_map: HashMap::new(),
            access_count_map: HashMap::new()
        }
    }

    /// Attempt to store a given file in the the cache.
    /// Storing will fail if the current files have more access attempts than the file being added.
    /// If the provided file has more more access attempts than one of the files in the cache,
    /// but the cache is full, a file will have to be removed from the cache to make room
    /// for the new file.
    pub fn try_store(&mut self, path: PathBuf, file: Arc<SizedFile>) -> result::Result<CacheInvalidationSuccess, CacheInvalidationError> {
        debug!("Possibly storing file: {:?} in the Cache.", path);

        let required_space_for_new_file: isize =  (self.size_bytes() as isize + file.size as isize) - self.size_limit as isize;

        // If there is negative required space, then we can just add the file to the cache, as it will fit.
        if required_space_for_new_file < 0 {
            debug!("Cache has room for the file.");
            self.file_map.insert(path, file);
            Ok(CacheInvalidationSuccess::InsertedFileIntoAvailableSpace)
        } else {
            // Otherwise, the cache will have to try to make some room for the new file

            let new_file_access_count: usize = *self.access_count_map.get(&path).unwrap_or(&0usize);
            let new_file_priority: usize = (self.priority_function)(new_file_access_count, file.size);


            match self.make_room_for_new_file(required_space_for_new_file as usize , new_file_priority) {
                Ok(_) => {
                    debug!("Made room in the cache for file and is now adding it");
                    self.file_map.insert(path, file);
                    Ok(CacheInvalidationSuccess::ReplacedFile)
                }
                Err(_) => {
                    debug!("The file does not have enough priority or is too large to be accepted into the cache.");
                    return Err(CacheInvalidationError::NewPriorityIsNotHighEnough);

                }
            }
        }
    }

    /// Remove the n lowest priority files to make room for a file with a size: required_space.
    ///
    /// If this returns an OK, this function has removed the required file space from the file_map.
    /// If this returns an Err, then either not enough space could be freed, or the priority of
    /// files that would need to be freed to make room for the new file is greater than the
    /// new file's priority, and as result no memory was freed.
    fn make_room_for_new_file(&mut self, required_space: usize, new_file_priority: usize) -> result::Result<(), String> { // TODO come up with a better result type.
        let mut possibly_freed_space: usize = 0;
        let mut priority_score_to_free: usize = 0;
        let mut file_paths_to_remove: Vec<PathBuf> = vec!();

        let mut priorities: Vec<(PathBuf,usize,usize)> = self.sorted_priorities();
        while possibly_freed_space < required_space {
            // pop the priority group with the lowest priority off of the vector
            match priorities.pop() {
                Some(lowest) => {
                    let (lowest_key, lowest_file_priority, lowest_file_size) = lowest;

                    possibly_freed_space += lowest_file_size;
                    priority_score_to_free += lowest_file_priority;
                    file_paths_to_remove.push(lowest_key.clone());

                    // Check if total priority to free is greater than the new file's priority,
                    // If it is, then don't free the files, as they in aggregate, are more important
                    // than the new file.
                    if priority_score_to_free > new_file_priority {
                        return Err(String::from("Priority of new file isn't higher than the aggregate priority of the file(s) it would replace"))
                    }
                },
                None => {
                    return Err(String::from("No more files to remove"))
                }
            };
        }

        // If this hasn't returned early, then the files to remove are less important than the new file.
        for file_key in file_paths_to_remove {
            self.file_map.remove(&file_key);
        }
        return Ok(());
    }

    ///Helper function that gets the file from the cache if it exists there.
    fn get(&mut self, path: &PathBuf) -> Option<CachedFile> {
        match self.file_map.get(path) {
            Some(sized_file) => {
                Some(
                    CachedFile {
                        path: path.clone(),
                        file: sized_file.clone()
                    }
                )
            }
            None => None // File not found
        }

    }

    /// Helper function for incrementing the access count for a given file name.
    ///
    /// This should only be used in cases where the file is known to exist, to avoid bloating the access count map with useless values.
    fn increment_access_count(&mut self, path: &PathBuf) {
        let count: &mut usize = self.access_count_map.entry(path.to_path_buf()).or_insert(0usize);
        *count += 1; // Increment the access count
    }

    /// Either gets the file from the cache, gets it from the filesystem and tries to cache it,
    /// or fails to find the file and returns None.
    pub fn get_or_cache(&mut self, pathbuf: PathBuf) -> Option<CachedFile> {
        trace!("{:#?}", self);
        // First, try to get the file in the cache that corresponds to the desired path.
        {
            if let Some(cache_file) = self.get(&pathbuf) {
                debug!("Cache hit for file: {:?}", pathbuf);
                self.increment_access_count(&pathbuf); // File is in the cache, increment the count
                return Some(cache_file)
            }
        }

        debug!("Cache missed for file: {:?}", pathbuf);
        // Instead the file needs to read from the filesystem.
        if let Ok(file) = SizedFile::open(pathbuf.as_path()) {
            self.increment_access_count(&pathbuf); // Because the file exists, but is not in the cache, increment the access count
            // If the file was read, convert it to a cached file and attempt to store it in the cache
            let arc_file: Arc<SizedFile> = Arc::new(file);
            let cached_file: CachedFile = CachedFile {
                path: pathbuf.clone(),
                file: arc_file.clone()
            };

            let _ = self.try_store(pathbuf, arc_file); // possibly stores the cached file in the store.
            Some(cached_file)
        } else {
            // Indicate that the file was not found in either the filesystem or cache.
            // This None is interpreted by Rocket by default to forward the request to its 404 handler.
            None
        }
    }

    /// Gets a tuple containing the Path, priority score, and size in bytes of the entry in
    /// the file_map with the lowest priority score.
    fn sorted_priorities(&self) -> Vec<(PathBuf,usize,usize)> {

        let mut priorities: Vec<(PathBuf,usize,usize)> = self.file_map.iter().map(|file| {
            let (file_key, sized_file) = file;
            let access_count: usize = self.access_count_map.get(file_key).unwrap_or(&1usize).clone();
            let size: usize = sized_file.size;
            let priority: usize = (self.priority_function)(access_count, size);

            (file_key.clone(), priority, size)
        }).collect();

        // Sort the priorities from highest priority to lowest, so when they are pop()ed later,
        // the last element will have the lowest priority.
        priorities.sort_by(|l,r| r.1.cmp(&l.1)); // sort by priority
//        println!("{:?}",priorities);
        priorities
    }


    /// Gets the size of the files that constitute the file_map.
    fn size_bytes(&self) -> usize {
        self.file_map.iter().fold(0usize, |size, x| {
            size +  x.1.size
        })
    }



    /// The default priority function used for determining if a file should be in the cache
    /// This function takes the square root of the size of the file times the number of times it has been accessed.
    fn balanced_priority(access_count: usize, size: usize ) -> usize {
        ((size as f64).sqrt() as usize) * access_count
    }
    pub const DEFAULT_PRIORITY_FUNCTION: PriorityFunction = Cache::balanced_priority;

    /// This priority function will value files in the cache based solely on the number of times the file is accessed.
    fn access_priority( access_count: usize, _ : usize) -> usize {
        access_count
    }
    pub const ACCESS_PRIORITY_FUNCTION: PriorityFunction = Cache::access_priority;

}


/// Custom type of function that is used to determine how to add files to the cache.
/// The first term will be assigned the access count of the file in question, while the second term will be assigned the size (in bytes) of the file in question.
/// The result will represent the priority of the file to remain in or be added to the cache.
/// The files with the largest priorities will be kept in the cache.
///
/// A closure that matches this type signature can be specified at cache instantiation to define how it will keep items in the cache.
pub type PriorityFunction = fn(usize, usize) -> usize;




#[cfg(test)]
mod tests {
    extern crate test;
    extern crate tempdir;
    extern crate rand;

    use self::tempdir::TempDir;

    use super::*;

    use std::sync::Mutex;
    use rocket::Rocket;
    use rocket::local::Client;
    use self::test::Bencher;
    use rocket::response::NamedFile;
    use rocket::State;
    use self::rand::{StdRng, Rng};
    use std::io::{Write, BufWriter};



    //    #[get("/<path..>", rank=4)]
//    fn cache_files(path: PathBuf, cache: State<Mutex<Cache>>) -> Option<CachedFile> {
//        let pathbuf: PathBuf = Path::new("test").join(path.clone()).to_owned();
//        cache.lock().unwrap().get_or_cache(pathbuf)
//    }
//    fn init_cache_rocket() -> Rocket {
//        let cache: Mutex<Cache> = Mutex::new(Cache::new(11000000)); // Cache can hold 11Mib
//        rocket::ignite()
//            .manage(cache)
//            .mount("/", routes![cache_files])
//    }
//
//    #[get("/<file..>")]
//    fn fs_files(file: PathBuf) -> Option<NamedFile> {
//        NamedFile::open(Path::new("test").join(file)).ok()
//    }
//    fn init_fs_rocket() -> Rocket {
//        rocket::ignite()
//            .mount("/", routes![fs_files])
//    }


    // generated by running `base64 /dev/urandom | head -c 1000000 > one_meg.txt`
    const ONE_MEG: &'static str = "one_meg.txt"; // file path used for testing
    const FIVE_MEGS: &'static str = "five_megs.txt"; // file path used for testing
    const TEN_MEGS: &'static str = "ten_megs.txt"; // file path used for testing

//
//    #[bench]
//    fn cache_access_1mib(b: &mut Bencher) {
//        let client = Client::new(init_cache_rocket()).expect("valid rocket instance");
//        let _response = client.get(ONE_MEG).dispatch(); // make sure the file is in the cache
//        b.iter(|| {
//            let mut response = client.get(ONE_MEG).dispatch();
//            let _body: Vec<u8> = response.body().unwrap().into_bytes().unwrap();
//        });
//    }
//
//    #[bench]
//    fn file_access_1mib(b: &mut Bencher) {
//        let client = Client::new(init_fs_rocket()).expect("valid rocket instance");
//        b.iter(|| {
//            let mut response = client.get(ONE_MEG).dispatch();
//            let _body: Vec<u8> = response.body().unwrap().into_bytes().unwrap();
//        });
//    }
//
//    #[bench]
//    fn cache_access_5mib(b: &mut Bencher) {
//        let client = Client::new(init_cache_rocket()).expect("valid rocket instance");
//        let _response = client.get(FIVE_MEGS).dispatch(); // make sure the file is in the cache
//        b.iter(|| {
//            let mut response = client.get(FIVE_MEGS).dispatch();
//            let _body: Vec<u8> = response.body().unwrap().into_bytes().unwrap();
//        });
//    }
//
//    #[bench]
//    fn file_access_5mib(b: &mut Bencher) {
//        let client = Client::new(init_fs_rocket()).expect("valid rocket instance");
//        b.iter(|| {
//            let mut response = client.get(FIVE_MEGS).dispatch();
//            let _body: Vec<u8> = response.body().unwrap().into_bytes().unwrap();
//        });
//    }
//
//    #[bench]
//    fn cache_access_10mib(b: &mut Bencher) {
//        let client = Client::new(init_cache_rocket()).expect("valid rocket instance");
//        let _response = client.get(TEN_MEGS).dispatch(); // make sure the file is in the cache
//        b.iter(|| {
//            let mut response = client.get(TEN_MEGS).dispatch();
//            let _body: Vec<u8> = response.body().unwrap().into_bytes().unwrap();
//        });
//    }
//
//    #[bench]
//    fn file_access_10mib(b: &mut Bencher) {
//        let client = Client::new(init_fs_rocket()).expect("valid rocket instance");
//        b.iter(|| {
//            let mut response = client.get(TEN_MEGS).dispatch();
//            let _body: Vec<u8> = response.body().unwrap().into_bytes().unwrap();
//        });
//    }

    // Comparison test
//    #[bench]
    fn clone5mib(b: &mut Bencher) {
        let mut megs2: Box<[u8; 5000000]> = Box::new([0u8; 5000000]);
        StdRng::new().unwrap().fill_bytes(megs2.as_mut());

        b.iter(|| {
            megs2.clone()
        });
    }

    #[test]
    fn file_exceeds_size_limit() {
        let mut cache: Cache = Cache::new(8000000); //Cache can hold only 8Mib
        let path: PathBuf = PathBuf::from("test/".to_owned()+TEN_MEGS);
        assert_eq!(cache.try_store(path.clone(), Arc::new(SizedFile::open(path.clone()).unwrap())), Err(CacheInvalidationError::NewPriorityIsNotHighEnough))
    }

    #[test]
    fn file_replaces_other_file() {
        let mut cache: Cache = Cache::new(5500000); //Cache can hold only 5.5Mib
        let path_5: PathBuf = PathBuf::from("test/".to_owned()+FIVE_MEGS);
        let path_1: PathBuf = PathBuf::from("test/".to_owned()+ONE_MEG);
        assert_eq!(
            cache.try_store(path_5.clone(), Arc::new(SizedFile::open(path_5.clone()).unwrap())),
            Ok(CacheInvalidationSuccess::InsertedFileIntoAvailableSpace)
        );
        cache.increment_access_count(&path_1); // increment the access count, causing it to have a higher priority the next time it tries to be stored.
        assert_eq!(
            cache.try_store(path_1.clone(), Arc::new(SizedFile::open(path_1.clone()).unwrap())),
            Err(CacheInvalidationError::NewPriorityIsNotHighEnough)
        );
        cache.increment_access_count(&path_1);
        assert_eq!(
            cache.try_store(path_1.clone(), Arc::new(SizedFile::open(path_1.clone()).unwrap())),
            Err(CacheInvalidationError::NewPriorityIsNotHighEnough)
        );
        cache.increment_access_count(&path_1);
        assert_eq!(
            cache.try_store(path_1.clone(), Arc::new(SizedFile::open(path_1.clone()).unwrap())),
            Ok(CacheInvalidationSuccess::ReplacedFile)
        );
    }

    const MEG1: usize = 1024 * 1024;
    const MEG2: usize = MEG1 * 2;
    const MEG3: usize = MEG1 * 3;
    const MEG5: usize = MEG1 * 5;
    const MEG10: usize = MEG1 * 10;

    const DIR_TEST: &'static str = "test1";
    const FILE_MEG1: &'static str = "meg1.txt";
    const FILE_MEG2: &'static str = "meg2.txt";
    const FILE_MEG3: &'static str = "meg3.txt";
    const FILE_MEG5: &'static str = "meg5.txt";
    const FILE_MEG10: &'static str = "meg10.txt";

    // Helper function that creates test files in a directory that is cleaned up after the test runs.
    fn create_test_file(temp_dir: &TempDir, size: usize, name: &str ) -> PathBuf {
        let path = temp_dir.path().join(name);
        let tmp_file = File::create(path.clone()).unwrap();
        let mut rand_data: Vec<u8> = vec![0u8; size];
        StdRng::new().unwrap().fill_bytes(rand_data.as_mut());
        let mut buffer = BufWriter::new(tmp_file);
        buffer.write(&rand_data).unwrap();
        path
    }

    #[test]
    fn new_file_replaces_lowest_priority_file() {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);
        let path_2m = create_test_file(&temp_dir, MEG2, FILE_MEG2);
//        let path_3m = create_test_file(&temp_dir, MEG3, FILE_MEG3);
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);
//        let path_10m = create_test_file(&temp_dir, MEG10, FILE_MEG10);

        let mut cache: Cache = Cache::new(MEG1 * 7 + 2000);

        cache.increment_access_count(&path_5m);
        assert_eq!(
            cache.try_store(path_5m.clone(), Arc::new(SizedFile::open(path_5m.clone()).unwrap())),
            Ok(CacheInvalidationSuccess::InsertedFileIntoAvailableSpace)
        );

        cache.increment_access_count(&path_2m);
        assert_eq!(
            cache.try_store(path_2m.clone(), Arc::new(SizedFile::open(path_2m.clone()).unwrap())),
            Ok(CacheInvalidationSuccess::InsertedFileIntoAvailableSpace)
        );

        // The cache will not accept the 1 meg file because sqrt(2)_size * 1_access is greater than sqrt(1)_size * 1_access
        cache.increment_access_count(&path_1m);
        assert_eq!(
            cache.try_store(path_1m.clone(), Arc::new(SizedFile::open(path_1m.clone()).unwrap())),
            Err(CacheInvalidationError::NewPriorityIsNotHighEnough)
        );

        // The cache will now accept the 1 meg file because (sqrt(2)_size * 1_access) for the old
        // file is less than (sqrt(1)_size * 2_access) for the new file.
        cache.increment_access_count(&path_1m);
        assert_eq!(
            cache.try_store(path_1m.clone(), Arc::new(SizedFile::open(path_1m.clone()).unwrap())),
            Ok(CacheInvalidationSuccess::ReplacedFile)
        );

        if let None = cache.get(&path_1m) {
            assert_eq!(&path_1m, &PathBuf::new()) // this will fail, this comparison is just for debugging a failure.
        }

        // Get directly from the cache, no FS involved.
        if let None = cache.get(&path_5m) {
            assert_eq!(&path_5m, &PathBuf::new()) // this will fail, this comparison is just for debugging a failure.
            // If this has failed, the cache removed the wrong file, implying the ordering of
            // priorities is wrong. It should remove the path_2m file instead.
        }

        if let Some(_) = cache.get(&path_2m) {
            assert_eq!(&path_2m, &PathBuf::new()) // this will fail, this comparison is just for debugging a failure.
        }
    }
}
