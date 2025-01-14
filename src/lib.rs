//! Runtime library of [ArceOS](https://github.com/rcore-os/arceos).
//!
//! Any application uses ArceOS should link this library. It does some
//! initialization work before entering the application's `main` function.
//!
//! # Cargo Features
//!
//! - `alloc`: Enable global memory allocator.
//! - `paging`: Enable page table manipulation support.
//! - `irq`: Enable interrupt handling support.
//! - `multitask`: Enable multi-threading support.
//! - `smp`: Enable SMP (symmetric multiprocessing) support.
//! - `fs`: Enable filesystem support.
//! - `net`: Enable networking support.
//! - `display`: Enable graphics support.
//!
//! All the features are optional and disabled by default.

#![cfg_attr(not(test), no_std)]
#![feature(doc_auto_cfg)]

#[macro_use]
extern crate axlog;

#[cfg(all(target_os = "none", not(test)))]
mod lang_items;

#[cfg(feature = "smp")]
mod mp;

use axhal::mem::MemTraverser;

#[cfg(feature = "smp")]
pub use self::mp::{entered_cpus_num, rust_main_secondary};

const LOGO: &str = r#"
       d8888                            .d88888b.   .d8888b.
      d88888                           d88P" "Y88b d88P  Y88b
     d88P888                           888     888 Y88b.
    d88P 888 888d888  .d8888b  .d88b.  888     888  "Y888b.
   d88P  888 888P"   d88P"    d8P  Y8b 888     888     "Y88b.
  d88P   888 888     888      88888888 888     888       "888
 d8888888888 888     Y88b.    Y8b.     Y88b. .d88P Y88b  d88P
d88P     888 888      "Y8888P  "Y8888   "Y88888P"   "Y8888P"
"#;

struct LogIfImpl;

#[crate_interface::impl_interface]
impl axlog::LogIf for LogIfImpl {
    fn console_write_str(s: &str) {
        axhal::console::write_bytes(s.as_bytes());
    }

    fn current_time() -> core::time::Duration {
        axhal::time::current_time()
    }

    fn current_cpu_id() -> Option<usize> {
        #[cfg(feature = "smp")]
        if is_init_ok() {
            Some(axhal::cpu::this_cpu_id())
        } else {
            None
        }
        #[cfg(not(feature = "smp"))]
        Some(0)
    }

    fn current_task_id() -> Option<u64> {
        if is_init_ok() {
            #[cfg(feature = "multitask")]
            {
                axtask::current_may_uninit().map(|curr| curr.id().as_u64())
            }
            #[cfg(not(feature = "multitask"))]
            None
        } else {
            None
        }
    }
}

use core::sync::atomic::{AtomicUsize, Ordering};

static INITED_CPUS: AtomicUsize = AtomicUsize::new(0);

/// Whether all CPUs has been initialized.
pub fn is_init_ok() -> bool {
    INITED_CPUS.load(Ordering::Acquire) == axconfig::SMP
}

/// The main entry point of the ArceOS runtime.
///
/// It is called from the bootstrapping code in [axhal]. `cpu_id` is the ID of
/// the current CPU, and `dtb` is the address of the device tree blob. It
/// finally calls the application's `main` function after all initialization
/// work is done.
///
/// In multi-core environment, this function is called on the primary CPU,
/// and the secondary CPUs call [`rust_main_secondary`].
#[cfg_attr(not(test), no_mangle)]
pub extern "C" fn rust_main(cpu_id: usize, dtb: usize) {
    ax_println!("{}", LOGO);
    ax_println!(
        "\
        arch = {}\n\
        platform = {}\n\
        target = {}\n\
        smp = {}\n\
        build_mode = {}\n\
        log_level = {}\n\
        ",
        option_env!("AX_ARCH").unwrap_or(""),
        option_env!("AX_PLATFORM").unwrap_or(""),
        option_env!("AX_TARGET").unwrap_or(""),
        option_env!("AX_SMP").unwrap_or(""),
        option_env!("AX_MODE").unwrap_or(""),
        option_env!("AX_LOG").unwrap_or(""),
    );

    info!("Logging is enabled.");
    info!("Primary CPU {} started, dtb = {:#x}.", cpu_id, dtb);
    info!("Platform name {}.", axhal::platform_name());

    info!("Found physcial memory regions:");
     axhal::mem::memory_regions( &MemTraverser{
        mapper: &|r|{
                info!(
                "  [{:x?}, {:x?}) {} ({:?})",
                r.paddr,
                r.paddr + r.size,
                r.name,
                r.flags
            );
            true
        },
    });
    #[cfg(feature = "alloc")]
    init_allocator();

    #[cfg(feature = "paging")]
    {
        info!("Initialize kernel page table...");
        remap_kernel_memory().expect("remap kernel memoy failed");
    }

    info!("Initialize platform devices...");
    axhal::platform_init();

    cfg_if::cfg_if! {
        if #[cfg(feature = "monolithic")] {
            axprocess::init_kernel_process();
        }
        else {
            #[cfg(feature = "multitask")]
            axtask::init_scheduler();
        }
    }
    #[cfg(any(feature = "fs", feature = "net", feature = "display"))]
    {
        #[allow(unused_variables)]
        let all_devices = axdriver::init_drivers();

        #[cfg(feature = "fs")]
        axfs::init_filesystems(all_devices.block);

        #[cfg(feature = "net")]
        axnet::init_network(all_devices.net);

        #[cfg(feature = "display")]
        axdisplay::init_display(all_devices.display);
    }

    #[cfg(feature = "irq")]
    {
        info!("Initialize interrupt handlers...");
        init_interrupt();
    }

    #[cfg(all(feature = "tls", not(feature = "multitask")))]
    {
        info!("Initialize thread local storage...");
        init_tls();
    }

    info!("Primary CPU {} init OK.", cpu_id);
    INITED_CPUS.fetch_add(1, Ordering::Relaxed);
}

/// exit the main task
pub fn exit_main() {
    #[cfg(feature = "multitask")]
    axtask::exit(0);
    #[cfg(not(feature = "multitask"))]
    {
        debug!("main task exited: exit_code={}", 0);
        axhal::misc::terminate();
    }
}

#[cfg(feature = "alloc")]
#[allow(dead_code)]
fn init_allocator() {
    use axhal::mem::{memory_regions, phys_to_virt, MemRegionFlags};

    info!("Initialize global memory allocator...");
    info!("  use {} allocator.", axalloc::global_allocator().name());

    let mut max_region_size = 0;
    let mut max_region_paddr = 0.into();
    // for r in memory_regions() {
    //     if r.flags.contains(MemRegionFlags::FREE) && r.size > max_region_size {
    //         max_region_size = r.size;
    //         max_region_paddr = r.paddr;
    //     }
    // }
    memory_regions(&MemTraverser { mapper: &|r| {
        if r.flags.contains(MemRegionFlags::FREE) && r.paddr == max_region_paddr {
            axalloc::global_init(phys_to_virt(r.paddr).as_usize(), r.size);
            false
        }
    }});
    // for r in memory_regions() {
    //     if r.flags.contains(MemRegionFlags::FREE) && r.paddr != max_region_paddr {
    //         axalloc::global_add_memory(phys_to_virt(r.paddr).as_usize(), r.size)
    //             .expect("add heap memory region failed");
    //     }
    // }
    memory_regions(&MemTraverser { mapper: &|r| {
        if r.flags.contains(MemRegionFlags::FREE) && r.paddr != max_region_paddr {
            axalloc::global_add_memory(phys_to_virt(r.paddr).as_usize(), r.size)
                .expect("add heap memory region failed");
        }
    }})
}

cfg_if::cfg_if! {
    if #[cfg(feature = "paging")] {
        use axhal::paging::PageTable;
        use lazy_init::LazyInit;
        /// The kernel page table.
        pub static KERNEL_PAGE_TABLE: LazyInit<PageTable> = LazyInit::new();

        fn remap_kernel_memory() -> Result<(), axhal::paging::PagingError> {
            use axhal::mem::{memory_regions, phys_to_virt};
            if axhal::cpu::this_cpu_is_bsp() {
                let mut kernel_page_table = PageTable::try_new()?;

                memory_regions(&MemTraverser { mapper: &|r|{
                    kernel_page_table.map_region(
                        phys_to_virt(r.paddr),
                        r.paddr,
                        r.size,
                        r.flags.into(),
                        true,
                    )?;
                } })

                #[cfg(feature = "img")]
                {
                    // 此时将测例加载到内存中，通过ramdisk和页表定向映射的方式来读取测例
                    use axconfig::{TESTCASE_MEMORY_START,TESTCASE_MEMORY_SIZE};
                    use axhal::mem::PhysAddr;
                    use axhal::mem::MemRegionFlags;
                    extern "C" {
                        fn img_start();
                    }
                    // 此时qemu运行，文件镜像的位置需要由汇编确定
                    let img_start_addr:PhysAddr = axhal::mem::virt_to_phys((img_start as usize).into());
                    kernel_page_table.map_region(
                        phys_to_virt(TESTCASE_MEMORY_START.into()),
                        img_start_addr,
                        TESTCASE_MEMORY_SIZE,
                        MemRegionFlags::from_bits(1 << 0 | 1 << 1 | 1 << 4).unwrap().into(),
                        true,
                    ).unwrap();
                }
                KERNEL_PAGE_TABLE.init_by(kernel_page_table);
            }

            unsafe { axhal::arch::write_page_table_root(KERNEL_PAGE_TABLE.root_paddr()) };
            Ok(())
        }
    }
}
#[cfg(feature = "irq")]
fn init_interrupt() {
    use axhal::time::TIMER_IRQ_NUM;

    // Setup timer interrupt handler
    const PERIODIC_INTERVAL_NANOS: u64 =
        axhal::time::NANOS_PER_SEC / axconfig::TICKS_PER_SEC as u64;

    #[percpu::def_percpu]
    static NEXT_DEADLINE: u64 = 0;

    fn update_timer() {
        let now_ns = axhal::time::current_time_nanos();
        // Safety: we have disabled preemption in IRQ handler.
        let mut deadline = unsafe { NEXT_DEADLINE.read_current_raw() };
        if now_ns >= deadline {
            deadline = now_ns + PERIODIC_INTERVAL_NANOS;
        }
        unsafe { NEXT_DEADLINE.write_current_raw(deadline + PERIODIC_INTERVAL_NANOS) };
        axhal::time::set_oneshot_timer(deadline);
    }

    axhal::irq::register_handler(TIMER_IRQ_NUM, || {
        update_timer();
        #[cfg(feature = "multitask")]
        axtask::on_timer_tick();
    });

    // Enable IRQs before starting app
    axhal::arch::enable_irqs();
}

#[cfg(all(feature = "tls", not(feature = "multitask")))]
fn init_tls() {
    let main_tls = axhal::tls::TlsArea::alloc();
    unsafe { axhal::arch::write_thread_pointer(main_tls.tls_ptr() as usize) };
    core::mem::forget(main_tls);
}
