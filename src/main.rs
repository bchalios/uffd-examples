use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use clap::Parser;
use log::{debug, info};
use rand::Rng;

use crate::mmap::{Mmap, MmapBuilder};
use crate::uffd::UffdManager;

mod mmap;
mod pagemap;
mod uffd;

#[derive(Parser, Debug)]
#[command(name = "uffd-tests")]
#[command(about = "Userfaultfd testing with configurable options", long_about = None)]
struct Args {
    /// Use huge pages (2MB) for memory allocation
    #[arg(long)]
    huge_pages: bool,
    /// Use hugetlbfs-backed file (requires hugetlbfs mount, e.g., /mnt/huge)
    #[arg(long, value_name = "PATH")]
    hugetlbfs_path: Option<PathBuf>,
    /// Pre-populate specific pages (e.g., --pre-populate 0 1 2)
    #[arg(long, num_args = 1.., value_name = "PAGE_INDEX")]
    pre_populate: Vec<usize>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("unnamed");
            writeln!(
                buf,
                "[{}] [{}] {}",
                thread_name,
                record.level(),
                record.args()
            )
        })
        .init();

    let args = Args::parse();

    debug!("Configuration:");
    debug!("  Huge pages: {}", args.huge_pages);
    debug!("  Hugetlbfs path: {:?}", args.hugetlbfs_path);
    debug!("  WP_ASYNC: {}", true);
    debug!("  Pre-populate pages: {:?}", args.pre_populate);

    // Build mmap with optional huge pages or hugetlbfs
    let mut mmap_builder = MmapBuilder::new();
    if let Some(hugetlbfs_path) = args.hugetlbfs_path {
        debug!("Using hugetlbfs-backed mapping from: {:?}", hugetlbfs_path);
        mmap_builder = mmap_builder.with_hugetlbfs(hugetlbfs_path);
    } else if args.huge_pages {
        debug!("Using anonymous huge pages");
        mmap_builder = mmap_builder.with_huge_pages();
    }
    let mmap = Arc::new(mmap_builder.build().unwrap());
    debug!("Using mmap: {:?}", mmap);

    // Pre-populate specified pages
    if !args.pre_populate.is_empty() {
        debug!("Pre-populating pages: {:?}", args.pre_populate);
        for &page_idx in &args.pre_populate {
            mmap.populate_read(page_idx);
        }
    }

    let mut mngr = UffdManager::new(mmap.clone());
    let uffd = mngr.start();

    info!("+---------------------------------------------+");
    info!("| 2 reads, 1 write and 1 don't need on page 0 |");
    info!("+---------------------------------------------+");
    let mut threads = vec![];
    threads.push(run_read_thread(mmap.clone(), 0));
    threads.push(run_read_thread(mmap.clone(), 0));
    threads.push(run_write_thread(mmap.clone(), 0));
    threads.push(run_dontneed_thread(mmap.clone(), 0));
    std::thread::sleep(Duration::from_secs(2));
    uffd.thaw();
    uffd.wait();

    for t in threads.drain(..) {
        info!("Joining {}", t.thread().name().unwrap());
        t.join().unwrap();
    }
    mmap.print_pm_info();

    info!("+---------------------------------------------+");
    info!("|      1 don't need and 1 write on page 0     |");
    info!("+---------------------------------------------+");
    threads.push(run_dontneed_thread(mmap.clone(), 0));
    threads.push(run_write_thread(mmap.clone(), 0));
    std::thread::sleep(Duration::from_secs(2));
    uffd.thaw();
    uffd.wait();

    for t in threads.drain(..) {
        info!("Joining {}", t.thread().name().unwrap());
        t.join().unwrap();
    }
    mmap.print_pm_info();

    info!("+---------------------------------------------+");
    info!("|              1 write on page 0              |");
    info!("+---------------------------------------------+");
    threads.push(run_write_thread(mmap.clone(), 0));
    std::thread::sleep(Duration::from_secs(2));
    uffd.thaw();
    uffd.wait();

    for t in threads.drain(..) {
        info!("Joining {}", t.thread().name().unwrap());
        t.join().unwrap();
    }
    mmap.print_pm_info();

    info!("+---------------------------------------------+");
    info!("|              1 read on page 1               |");
    info!("+---------------------------------------------+");
    threads.push(run_read_thread(mmap.clone(), 1));
    std::thread::sleep(Duration::from_secs(2));
    uffd.thaw();
    uffd.wait();

    for t in threads.drain(..) {
        info!("Joining {}", t.thread().name().unwrap());
        t.join().unwrap();
    }

    mmap.print_pm_info();

    uffd.stop();
}

fn run_read_thread(mmap: Arc<Mmap>, page_idx: usize) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("read".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(rand::thread_rng().gen_range(0..10)));
            mmap.read(page_idx);
        })
        .unwrap()
}

fn run_write_thread(mmap: Arc<Mmap>, page_idx: usize) -> JoinHandle<()> {
    let t = std::thread::Builder::new()
        .name("write".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(rand::thread_rng().gen_range(0..10)));
            mmap.write(page_idx, &vec![0x42; mmap.page_size]);
        })
        .unwrap();
    info!("Done writing");
    t
}

fn run_dontneed_thread(mmap: Arc<Mmap>, page_idx: usize) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("dont-need-thread".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(rand::thread_rng().gen_range(0..10)));
            mmap.dont_need(page_idx);
        })
        .unwrap()
}
