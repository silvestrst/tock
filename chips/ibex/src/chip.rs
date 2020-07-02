//! High-level setup and interrupt mapping for the chip.

use core::fmt::Write;
use core::hint::unreachable_unchecked;

use kernel;
use kernel::common::registers::FieldValue;
use kernel::common::StaticRef;
use kernel::debug;
use kernel::Chip;
use rv32i::csr::{mcause, mie::mie, mip::mip, mtvec::mtvec, CSR};
use rv32i::syscall::SysCall;

use crate::gpio;
use crate::hmac;
use crate::interrupts;
use crate::plic;
use crate::pwrmgr;
use crate::timer;
use crate::uart;
use crate::usbdev;

use lowrisc::uart::{Uart, UartRegisters};

pub const CHIP_FREQ: u32 = 50_000_000;

// Chip configuration based on the target device.
pub struct Config {
    pub name: &'static str,
    pub chip_freq: u32,
    pub uart_baudrate: u32,
}

pub struct Peripherals<'a> {
    pub uart0: Uart<'a>,
}

pub struct Ibex<'a> {
    userspace_kernel_boundary: SysCall,
    pmp: rv32i::pmp::PMPConfig,
    pub peripherals: Peripherals<'a>,
    pub chip_config: Config,
}

impl<'a> Ibex<'a> {
    pub unsafe fn new(config: Config) -> Ibex<'a> {
        let uart0_base: StaticRef<UartRegisters> =
            StaticRef::new(0x4000_0000 as *const UartRegisters);

        Ibex {
            userspace_kernel_boundary: SysCall::new(),
            pmp: rv32i::pmp::PMPConfig::new(4),
            peripherals: Peripherals {
                uart0: Uart::new(uart0_base, config.chip_freq),
            },
            chip_config: config,
        }
    }

    pub unsafe fn enable_plic_interrupts(&self) {
        plic::disable_all();
        plic::clear_all_pending();
        plic::enable_all();
    }

    unsafe fn handle_plic_interrupts(&self) {
        while let Some(interrupt) = plic::next_pending() {
            match interrupt {
                interrupts::UART_TX_WATERMARK..=interrupts::UART_RX_PARITY_ERR => {
                    uart::UART0.handle_interrupt()
                }
                int_pin @ interrupts::GPIO_PIN0..=interrupts::GPIO_PIN31 => {
                    let pin = &gpio::PORT[(int_pin - interrupts::GPIO_PIN0) as usize];
                    pin.handle_interrupt();
                }
                interrupts::HMAC_HMAC_DONE..=interrupts::HMAC_HMAC_ERR => {
                    hmac::HMAC.handle_interrupt()
                }
                interrupts::USBDEV_PKT_RECEIVED..=interrupts::USBDEV_CONNECTED => {
                    usbdev::USB.handle_interrupt()
                }
                interrupts::PWRMGRWAKEUP => {
                    pwrmgr::PWRMGR.handle_interrupt();
                    self.check_until_true_or_interrupt(
                        || pwrmgr::PWRMGR.check_clock_propagation(),
                        None,
                    );
                }
                _ => debug!("Pidx {}", interrupt),
            }
            plic::complete(interrupt);
        }
    }

    /// Run a function in an interruptable loop.
    ///
    /// The function will run until it returns true, an interrupt occurs or if
    /// `max_tries` is not `None` and that limit is reached.
    /// If the function returns true this call will also return true. If an
    /// interrupt occurs or `max_tries` is reached this call will return false.
    fn check_until_true_or_interrupt<F>(&self, f: F, max_tries: Option<usize>) -> bool
    where
        F: Fn() -> bool,
    {
        match max_tries {
            Some(t) => {
                for _i in 0..t {
                    if self.has_pending_interrupts() {
                        return false;
                    }
                    if f() {
                        return true;
                    }
                }
            }
            None => {
                while !self.has_pending_interrupts() {
                    if f() {
                        return true;
                    }
                }
            }
        }

        false
    }
}

impl kernel::Chip for Ibex<'_> {
    type MPU = rv32i::pmp::PMPConfig;
    type UserspaceKernelBoundary = SysCall;
    type SysTick = ();

    fn mpu(&self) -> &Self::MPU {
        &self.pmp
    }

    fn systick(&self) -> &Self::SysTick {
        &()
    }

    fn userspace_kernel_boundary(&self) -> &SysCall {
        &self.userspace_kernel_boundary
    }

    fn service_pending_interrupts(&self) {
        let mut reenable_intr = FieldValue::<u32, mie::Register>::new(0, 0, 0);

        loop {
            let mip = CSR.mip.extract();

            if mip.is_set(mip::mtimer) {
                unsafe {
                    timer::TIMER.service_interrupt();
                }
                reenable_intr += mie::mtimer::SET;
            }
            if mip.is_set(mip::mext) {
                unsafe {
                    self.handle_plic_interrupts();
                }
                reenable_intr += mie::mext::SET;
            }

            if !mip.matches_any(mip::mext::SET + mip::mtimer::SET) {
                break;
            }
        }

        // re-enable any interrupt classes which we handled
        CSR.mie.modify(reenable_intr);
    }

    fn has_pending_interrupts(&self) -> bool {
        let mip = CSR.mip.extract();
        mip.matches_any(mip::mext::SET + mip::mtimer::SET)
    }

    fn sleep(&self) {
        unsafe {
            pwrmgr::PWRMGR.enable_low_power();
            self.check_until_true_or_interrupt(|| pwrmgr::PWRMGR.check_clock_propagation(), None);
            rv32i::support::wfi();
        }
    }

    unsafe fn atomic<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        rv32i::support::atomic(f)
    }

    unsafe fn print_state(&self, writer: &mut dyn Write) {
        rv32i::print_riscv_state(writer);
    }
}

fn handle_exception(exception: mcause::Exception) {
    match exception {
        mcause::Exception::UserEnvCall | mcause::Exception::SupervisorEnvCall => (),

        mcause::Exception::InstructionMisaligned
        | mcause::Exception::InstructionFault
        | mcause::Exception::IllegalInstruction
        | mcause::Exception::Breakpoint
        | mcause::Exception::LoadMisaligned
        | mcause::Exception::LoadFault
        | mcause::Exception::StoreMisaligned
        | mcause::Exception::StoreFault
        | mcause::Exception::MachineEnvCall
        | mcause::Exception::InstructionPageFault
        | mcause::Exception::LoadPageFault
        | mcause::Exception::StorePageFault
        | mcause::Exception::Unknown => {
            panic!("fatal exception");
        }
    }
}

unsafe fn handle_interrupt(intr: mcause::Interrupt) {
    match intr {
        mcause::Interrupt::UserSoft
        | mcause::Interrupt::UserTimer
        | mcause::Interrupt::UserExternal => {
            debug!("unexpected user-mode interrupt");
        }
        mcause::Interrupt::SupervisorExternal
        | mcause::Interrupt::SupervisorTimer
        | mcause::Interrupt::SupervisorSoft => {
            debug!("unexpected supervisor-mode interrupt");
        }

        mcause::Interrupt::MachineSoft => {
            CSR.mie.modify(mie::msoft::CLEAR);
        }
        mcause::Interrupt::MachineTimer => {
            CSR.mie.modify(mie::mtimer::CLEAR);
        }
        mcause::Interrupt::MachineExternal => {
            CSR.mie.modify(mie::mext::CLEAR);
        }

        mcause::Interrupt::Unknown => {
            debug!("interrupt of unknown cause");
        }
    }
}

/// Trap handler for board/chip specific code.
///
/// For the Ibex this gets called when an interrupt occurs while the chip is
/// in kernel mode. All we need to do is check which interrupt occurred and
/// disable it.
#[export_name = "_start_trap_rust"]
pub unsafe extern "C" fn start_trap_rust() {
    match mcause::Trap::from(CSR.mcause.extract()) {
        mcause::Trap::Interrupt(interrupt) => {
            handle_interrupt(interrupt);
        }
        mcause::Trap::Exception(exception) => {
            handle_exception(exception);
        }
    }
}

/// Function that gets called if an interrupt occurs while an app was running.
/// mcause is passed in, and this function should correctly handle disabling the
/// interrupt that fired so that it does not trigger again.
#[export_name = "_disable_interrupt_trap_handler"]
pub unsafe extern "C" fn disable_interrupt_trap_handler(mcause_val: u32) {
    match mcause::Trap::from(mcause_val) {
        mcause::Trap::Interrupt(interrupt) => {
            handle_interrupt(interrupt);
        }
        _ => {
            panic!("unexpected non-interrupt\n");
        }
    }
}

pub unsafe fn configure_trap_handler() {
    // The Ibex CPU does not support non-vectored trap entries.
    CSR.mtvec
        .write(mtvec::trap_addr.val(_start_trap_vectored as u32 >> 2) + mtvec::mode::Vectored)
}

// Mock implementation for crate tests that does not include the section
// specifier, as the test will not use our linker script, and the host
// compilation environment may not allow the section name.
#[cfg(not(any(target_arch = "riscv32", target_os = "none")))]
pub extern "C" fn _start_trap_vectored() {
    unsafe {
        unreachable_unchecked();
    }
}

#[cfg(all(target_arch = "riscv32", target_os = "none"))]
#[link_section = ".riscv.trap_vectored"]
#[export_name = "_start_trap_vectored"]
#[naked]
pub extern "C" fn _start_trap_vectored() -> ! {
    unsafe {
        // According to the Ibex user manual:
        // [NMI] has interrupt ID 31, i.e., it has the highest priority of all
        // interrupts and the core jumps to the trap-handler base address (in
        // mtvec) plus 0x7C to handle the NMI.
        //
        // Below are 32 (non-compressed) jumps to cover the entire possible
        // range of vectored traps.
        #[cfg(all(target_arch = "riscv32", target_os = "none"))]
        llvm_asm!("
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
            j _start_trap
        "
        :
        :
        :
        : "volatile");
        unreachable_unchecked()
    }
}
