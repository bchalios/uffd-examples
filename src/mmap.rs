use std::{
    ffi::{CString, c_void},
    path::PathBuf,
};

use libc::c_int;
use log::info;

use crate::pagemap::{PagemapEntry, PagemapReader};

pub struct MmapBuilder {
    flags: c_int,
    prot: c_int,
    page_size: usize,
    hugetlbfs_path: Option<PathBuf>,
}

impl MmapBuilder {
    pub fn new() -> Self {
        MmapBuilder {
            flags: libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
            prot: libc::PROT_READ | libc::PROT_WRITE,
            page_size: 4 * 1024,
            hugetlbfs_path: None,
        }
    }

    pub fn with_huge_pages(mut self) -> Self {
        self.flags |= libc::MAP_HUGETLB | libc::MAP_HUGE_2MB;
        self.page_size = 2 * 1024 * 1024;
        self
    }

    pub fn with_hugetlbfs(mut self, path: PathBuf) -> Self {
        self.flags = libc::MAP_SHARED;
        self.hugetlbfs_path = Some(path);
        self.page_size = 2 * 1024 * 1024; // hugetlbfs uses 2MB pages
        self
    }

    pub fn build(self) -> Result<Mmap, std::io::Error> {
        let len = 10 * self.page_size;

        let (addr, fd) = if let Some(hugetlbfs_path) = self.hugetlbfs_path {
            // Create a file in hugetlbfs
            let file_path = hugetlbfs_path.join(format!("uffd-test-{}", std::process::id()));
            let path_cstr = CString::new(file_path.to_str().unwrap()).unwrap();

            let fd = unsafe {
                libc::open(
                    path_cstr.as_ptr(),
                    libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                    0o600,
                )
            };

            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }

            // Allocate the file to the desired size
            let ret = unsafe { libc::ftruncate(fd, len as i64) };
            if ret < 0 {
                unsafe { libc::close(fd) };
                return Err(std::io::Error::last_os_error());
            }

            // Unlink the file immediately (it will be deleted when closed)
            unsafe { libc::unlink(path_cstr.as_ptr()) };

            // Map the hugetlbfs file (use MAP_SHARED for file-backed)
            let addr =
                unsafe { libc::mmap(std::ptr::null_mut(), len, self.prot, self.flags, fd, 0) };

            if addr == libc::MAP_FAILED {
                unsafe { libc::close(fd) };
                return Err(std::io::Error::last_os_error());
            }

            (addr, Some(fd))
        } else {
            // Anonymous mapping
            let addr =
                unsafe { libc::mmap(std::ptr::null_mut(), len, self.prot, self.flags, -1, 0) };

            if addr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }

            (addr, None)
        };

        Ok(Mmap {
            addr,
            len,
            page_size: self.page_size,
            _fd: fd,
            pm_reader: PagemapReader::new(self.page_size).unwrap(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageState {
    None,
    Faulted,
    Removed,
}

#[derive(Debug)]
pub struct Mmap {
    pub addr: *mut c_void,
    pub len: usize,
    pub page_size: usize,
    _fd: Option<c_int>,
    pm_reader: PagemapReader,
}

unsafe impl Sync for Mmap {}
unsafe impl Send for Mmap {}

impl Mmap {
    pub fn page_idx(&self, addr: *const c_void) -> usize {
        assert!((addr as usize).is_multiple_of(self.page_size));
        (unsafe { addr.offset_from_unsigned(self.addr) }) / self.page_size
    }

    pub fn read(&self, page_idx: usize) -> Vec<u8> {
        info!("Reading page {page_idx}");
        assert!(page_idx < self.len / self.page_size);
        let slice = unsafe {
            std::slice::from_raw_parts(
                self.addr.cast::<u8>().add(page_idx * self.page_size),
                self.page_size,
            )
        };

        let mut ret = vec![0u8; self.page_size];
        ret.copy_from_slice(slice);
        ret
    }

    pub fn populate_read(&self, page_idx: usize) {
        info!("populating page {page_idx}");
        assert!(page_idx < self.len / self.page_size);
        let ret = unsafe {
            libc::madvise(
                self.addr
                    .cast::<u8>()
                    .add(page_idx * self.page_size)
                    .cast::<_>(),
                self.page_size,
                libc::MADV_POPULATE_READ,
            )
        };
        assert_eq!(ret, 0);
    }

    pub fn write(&self, page_idx: usize, data: &[u8]) {
        info!("Writing page {page_idx}");
        assert!(page_idx < self.len / self.page_size);
        let slice = unsafe {
            std::slice::from_raw_parts_mut(
                self.addr.cast::<u8>().add(page_idx * self.page_size),
                self.page_size,
            )
        };

        slice.copy_from_slice(data);
    }

    pub fn dont_need(&self, page_idx: usize) {
        info!("Calling madvise(DONTNEED) on page {page_idx}");
        assert!(page_idx < self.len / self.page_size);

        let ret = unsafe {
            libc::madvise(
                self.addr.add(page_idx * self.page_size),
                self.page_size,
                libc::MADV_DONTNEED,
            )
        };

        assert_eq!(ret, 0);
    }

    pub fn pm_info(&self, page_idx: usize) -> PagemapEntry {
        assert!(page_idx < self.len / self.page_size);
        self.pm_reader
            .read_entry(unsafe { self.addr.add(page_idx * self.page_size) as usize })
            .unwrap()
    }

    pub fn print_pm_info(&self) {
        let nr_pages = self.len / self.page_size;
        for i in 0..nr_pages {
            let pm_entry = self.pm_info(i);
            let present = pm_entry.is_present();
            let wp = pm_entry.is_write_protected();
            let dirty = present & !wp;
            info!("PageMap info page {i}: present: {present} write-protected: {wp} dirty: {dirty}",);
        }
    }
}
