use axhal::{arch::TrapFrame, mem::VirtAddr, paging::MappingFlags};

struct TrapHandlerImpl;

#[crate_interface::impl_interface]
impl axhal::trap::TrapHandler for TrapHandlerImpl {
    fn handle_irq(_irq_num: usize) {
        #[cfg(feature = "irq")]
        {
            let guard = kernel_guard::NoPreempt::new();
            // trap进来，统计时间信息
            // axprocess::time_stat_from_user_to_kernel();
            axhal::irq::dispatch_irq(_irq_num);
            // axprocess::time_stat_from_kernel_to_user();
            drop(guard); // rescheduling may occur when preemption is re-enabled.
        }
    }
    fn handle_syscall(_syscall_id: usize, _args: [usize; 6]) -> isize {
        // axprocess::time_stat_from_user_to_kernel();
        // let ans = syscall(syscall_id, args);
        // axprocess::time_stat_from_kernel_to_user();
        // ans
        unimplemented!()
        // 0
    }

    #[cfg(feature = "paging")]
    fn handle_page_fault(_addr: VirtAddr, _flags: MappingFlags, _tf: &mut TrapFrame) {
        // use axprocess::handle_page_fault;
        // use axsignal::signal_no::SignalNo;
        // use axtask::current;
        // axprocess::time_stat_from_user_to_kernel();
        // use crate::syscall::signal::{syscall_sigreturn, syscall_tkill};
        // if addr.as_usize() == SIGNAL_RETURN_TRAP {
        //     // 说明是信号执行完毕，此时应当执行sig return
        //     tf.regs.a0 = syscall_sigreturn() as usize;
        //     return;
        // }

        // if handle_page_fault(addr, flags).is_err() {
        //     // 如果处理失败，则发出sigsegv信号
        //     let curr = current().id().as_u64() as isize;
        //     axlog::error!("kill task: {}", curr);
        //     syscall_tkill(curr, SignalNo::SIGSEGV as isize);
        // }
        // axprocess::time_stat_from_kernel_to_user();
        panic!();
        // axlog::ax_println!("tes");
    }

    // #[cfg(feature = "signal")]
    // fn handle_signal() {
    //     axprocess::handle_signals();
    // }
}
