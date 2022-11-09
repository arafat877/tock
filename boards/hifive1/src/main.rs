// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Board file for SiFive HiFive1b RISC-V development platform.
//!
//! - <https://www.sifive.com/boards/hifive1-rev-b>
//!
//! This board file is only compatible with revision B of the HiFive1.

#![no_std]
// Disable this attribute when documenting, as a workaround for
// https://github.com/rust-lang/rust/issues/62184.
#![cfg_attr(not(doc), no_main)]

use capsules::virtual_alarm::{MuxAlarm, VirtualMuxAlarm};
use e310_g002::interrupt_service::E310G002DefaultPeripherals;
use kernel::capabilities;
use kernel::component::Component;
use kernel::dynamic_deferred_call::{DynamicDeferredCall, DynamicDeferredCallClientState};
use kernel::hil;
use kernel::hil::led::LedLow;
use kernel::platform::chip::Chip;
use kernel::platform::scheduler_timer::VirtualSchedulerTimer;
use kernel::platform::{KernelResources, SyscallDriverLookup};
use kernel::scheduler::cooperative::CooperativeSched;
use kernel::utilities::registers::interfaces::ReadWriteable;
use kernel::Kernel;
use kernel::{create_capability, debug, static_init};
use rv32i::csr;

pub mod io;

pub const NUM_PROCS: usize = 4;
//
// Actual memory for holding the active process structures. Need an empty list
// at least.
static mut PROCESSES: [Option<&'static dyn kernel::process::Process>; NUM_PROCS] =
    [None; NUM_PROCS];

// Reference to the chip for panic dumps.
static mut CHIP: Option<&'static e310_g002::chip::E310x<E310G002DefaultPeripherals>> = None;
// Reference to the process printer for panic dumps.
static mut PROCESS_PRINTER: Option<&'static kernel::process::ProcessPrinterText> = None;

// How should the kernel respond when a process faults.
const FAULT_RESPONSE: kernel::process::PanicFaultPolicy = kernel::process::PanicFaultPolicy {};

/// Dummy buffer that causes the linker to reserve enough space for the stack.
#[no_mangle]
#[link_section = ".stack_buffer"]
pub static mut STACK_MEMORY: [u8; 0x900] = [0; 0x900];

/// A structure representing this platform that holds references to all
/// capsules for this platform. We've included an alarm and console.
struct HiFive1 {
    led: &'static capsules::led::LedDriver<
        'static,
        LedLow<'static, sifive::gpio::GpioPin<'static>>,
        3,
    >,
    console: &'static capsules::console::Console<'static>,
    lldb: &'static capsules::low_level_debug::LowLevelDebug<
        'static,
        capsules::virtual_uart::UartDevice<'static>,
    >,
    alarm: &'static capsules::alarm::AlarmDriver<
        'static,
        VirtualMuxAlarm<'static, sifive::clint::Clint<'static>>,
    >,
    scheduler: &'static CooperativeSched<'static>,
    scheduler_timer:
        &'static VirtualSchedulerTimer<VirtualMuxAlarm<'static, sifive::clint::Clint<'static>>>,
}

/// Mapping of integer syscalls to objects that implement syscalls.
impl SyscallDriverLookup for HiFive1 {
    fn with_driver<F, R>(&self, driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<&dyn kernel::syscall::SyscallDriver>) -> R,
    {
        match driver_num {
            capsules::led::DRIVER_NUM => f(Some(self.led)),
            capsules::console::DRIVER_NUM => f(Some(self.console)),
            capsules::alarm::DRIVER_NUM => f(Some(self.alarm)),
            capsules::low_level_debug::DRIVER_NUM => f(Some(self.lldb)),
            _ => f(None),
        }
    }
}

impl KernelResources<e310_g002::chip::E310x<'static, E310G002DefaultPeripherals<'static>>>
    for HiFive1
{
    type SyscallDriverLookup = Self;
    type SyscallFilter = ();
    type ProcessFault = ();
    type CredentialsCheckingPolicy = ();
    type Scheduler = CooperativeSched<'static>;
    type SchedulerTimer =
        VirtualSchedulerTimer<VirtualMuxAlarm<'static, sifive::clint::Clint<'static>>>;
    type WatchDog = ();
    type ContextSwitchCallback = ();

    fn syscall_driver_lookup(&self) -> &Self::SyscallDriverLookup {
        &self
    }
    fn syscall_filter(&self) -> &Self::SyscallFilter {
        &()
    }
    fn process_fault(&self) -> &Self::ProcessFault {
        &()
    }
    fn credentials_checking_policy(&self) -> &'static Self::CredentialsCheckingPolicy {
        &()
    }
    fn scheduler(&self) -> &Self::Scheduler {
        self.scheduler
    }
    fn scheduler_timer(&self) -> &Self::SchedulerTimer {
        &self.scheduler_timer
    }
    fn watchdog(&self) -> &Self::WatchDog {
        &()
    }
    fn context_switch_callback(&self) -> &Self::ContextSwitchCallback {
        &()
    }
}

/// This is in a separate, inline(never) function so that its stack frame is
/// removed when this function returns. Otherwise, the stack space used for
/// these static_inits is wasted.
/// Additionally, this function should only ever be called once, as it is declaring and
/// initializing static memory for system peripherals.
#[inline(never)]
unsafe fn create_peripherals() -> &'static mut E310G002DefaultPeripherals<'static> {
    static_init!(
        E310G002DefaultPeripherals,
        E310G002DefaultPeripherals::new()
    )
}

/// For the HiFive1, if load_process is inlined, it leads to really large stack utilization in
/// main. By wrapping it in a non-inlined function, this reduces the stack utilization once
/// processes are running.
#[inline(never)]
fn load_processes_not_inlined<C: Chip>(board_kernel: &'static Kernel, chip: &'static C) {
    // These symbols are defined in the linker script.
    extern "C" {
        /// Beginning of the ROM region containing app images.
        static _sapps: u8;
        /// End of the ROM region containing app images.
        static _eapps: u8;
        /// Beginning of the RAM region for app memory.
        static mut _sappmem: u8;
        /// End of the RAM region for app memory.
        static _eappmem: u8;
    }

    let app_flash = unsafe {
        core::slice::from_raw_parts(
            &_sapps as *const u8,
            &_eapps as *const u8 as usize - &_sapps as *const u8 as usize,
        )
    };

    let app_memory = unsafe {
        core::slice::from_raw_parts_mut(
            &mut _sappmem as *mut u8,
            &_eappmem as *const u8 as usize - &_sappmem as *const u8 as usize,
        )
    };

    let process_mgmt_cap = create_capability!(capabilities::ProcessManagementCapability);
    kernel::process::load_processes(
        board_kernel,
        chip,
        app_flash,
        app_memory,
        unsafe { &mut PROCESSES },
        &FAULT_RESPONSE,
        &process_mgmt_cap,
    )
    .unwrap_or_else(|err| {
        debug!("Error loading processes!");
        debug!("{:?}", err);
    });
}

/// Main function.
///
/// This function is called from the arch crate after some very basic RISC-V
/// setup and RAM initialization.
#[no_mangle]
pub unsafe fn main() {
    // only machine mode
    rv32i::configure_trap_handler(rv32i::PermissionMode::Machine);

    let peripherals = create_peripherals();

    peripherals.e310x.watchdog.disable();
    peripherals.e310x.rtc.disable();
    peripherals.e310x.pwm0.disable();
    peripherals.e310x.pwm1.disable();
    peripherals.e310x.pwm2.disable();
    peripherals.e310x.uart1.disable();

    peripherals
        .e310x
        .prci
        .set_clock_frequency(sifive::prci::ClockFrequency::Freq16Mhz);

    let main_loop_cap = create_capability!(capabilities::MainLoopCapability);

    let board_kernel = static_init!(kernel::Kernel, kernel::Kernel::new(&PROCESSES));

    let dynamic_deferred_call_clients =
        static_init!([DynamicDeferredCallClientState; 2], Default::default());
    let dynamic_deferred_caller = static_init!(
        DynamicDeferredCall,
        DynamicDeferredCall::new(dynamic_deferred_call_clients)
    );
    DynamicDeferredCall::set_global_instance(dynamic_deferred_caller);

    // Configure kernel debug gpios as early as possible
    kernel::debug::assign_gpios(
        Some(&peripherals.e310x.gpio_port[22]), // Red
        None,
        None,
    );

    // Create a shared UART channel for the console and for kernel debug.
    let uart_mux = components::console::UartMuxComponent::new(
        &peripherals.e310x.uart0,
        115200,
        dynamic_deferred_caller,
    )
    .finalize(components::uart_mux_component_static!());

    // LEDs
    let led = components::led::LedsComponent::new().finalize(components::led_component_static!(
        LedLow<'static, sifive::gpio::GpioPin>,
        LedLow::new(&peripherals.e310x.gpio_port[22]), // Red
        LedLow::new(&peripherals.e310x.gpio_port[19]), // Green
        LedLow::new(&peripherals.e310x.gpio_port[21]), // Blue
    ));

    peripherals.e310x.uart0.initialize_gpio_pins(
        &peripherals.e310x.gpio_port[17],
        &peripherals.e310x.gpio_port[16],
    );

    let hardware_timer = static_init!(
        sifive::clint::Clint,
        sifive::clint::Clint::new(&e310_g002::clint::CLINT_BASE)
    );

    // Create a shared virtualization mux layer on top of a single hardware
    // alarm.
    let mux_alarm = static_init!(
        MuxAlarm<'static, sifive::clint::Clint>,
        MuxAlarm::new(hardware_timer)
    );
    hil::time::Alarm::set_alarm_client(hardware_timer, mux_alarm);

    // Alarm
    let virtual_alarm_user = static_init!(
        VirtualMuxAlarm<'static, sifive::clint::Clint>,
        VirtualMuxAlarm::new(mux_alarm)
    );
    virtual_alarm_user.setup();

    let systick_virtual_alarm = static_init!(
        VirtualMuxAlarm<'static, sifive::clint::Clint>,
        VirtualMuxAlarm::new(mux_alarm)
    );
    systick_virtual_alarm.setup();

    let memory_allocation_cap = create_capability!(capabilities::MemoryAllocationCapability);
    let alarm = static_init!(
        capsules::alarm::AlarmDriver<'static, VirtualMuxAlarm<'static, sifive::clint::Clint>>,
        capsules::alarm::AlarmDriver::new(
            virtual_alarm_user,
            board_kernel.create_grant(capsules::alarm::DRIVER_NUM, &memory_allocation_cap)
        )
    );
    hil::time::Alarm::set_alarm_client(virtual_alarm_user, alarm);

    let chip = static_init!(
        e310_g002::chip::E310x<E310G002DefaultPeripherals>,
        e310_g002::chip::E310x::new(peripherals, hardware_timer)
    );
    CHIP = Some(chip);

    let process_printer = components::process_printer::ProcessPrinterTextComponent::new()
        .finalize(components::process_printer_text_component_static!());
    PROCESS_PRINTER = Some(process_printer);

    let process_console = components::process_console::ProcessConsoleComponent::new(
        board_kernel,
        uart_mux,
        mux_alarm,
        process_printer,
    )
    .finalize(components::process_console_component_static!(
        sifive::clint::Clint
    ));
    let _ = process_console.start();

    // Need to enable all interrupts for Tock Kernel
    chip.enable_plic_interrupts();

    // enable interrupts globally
    csr::CSR
        .mie
        .modify(csr::mie::mie::mext::SET + csr::mie::mie::msoft::SET + csr::mie::mie::mtimer::SET);
    csr::CSR.mstatus.modify(csr::mstatus::mstatus::mie::SET);

    // Setup the console.
    let console = components::console::ConsoleComponent::new(
        board_kernel,
        capsules::console::DRIVER_NUM,
        uart_mux,
    )
    .finalize(components::console_component_static!());
    // Create the debugger object that handles calls to `debug!()`.
    components::debug_writer::DebugWriterComponent::new(uart_mux)
        .finalize(components::debug_writer_component_static!());

    let lldb = components::lldb::LowLevelDebugComponent::new(
        board_kernel,
        capsules::low_level_debug::DRIVER_NUM,
        uart_mux,
    )
    .finalize(components::low_level_debug_component_static!());

    // Need two debug!() calls to actually test with QEMU. QEMU seems to have a
    // much larger UART TX buffer (or it transmits faster). With a single call
    // the entire message is printed to console even if the kernel loop does not run
    debug!("HiFive1 initialization complete.");
    debug!("Entering main loop.");

    let scheduler = components::sched::cooperative::CooperativeComponent::new(&PROCESSES)
        .finalize(components::cooperative_component_static!(NUM_PROCS));

    let scheduler_timer = static_init!(
        VirtualSchedulerTimer<VirtualMuxAlarm<'static, sifive::clint::Clint<'static>>>,
        VirtualSchedulerTimer::new(systick_virtual_alarm)
    );

    let hifive1 = HiFive1 {
        console: console,
        alarm: alarm,
        lldb: lldb,
        led,
        scheduler,
        scheduler_timer,
    };

    load_processes_not_inlined(board_kernel, chip);

    board_kernel.kernel_loop(&hifive1, chip, None::<&kernel::ipc::IPC<0>>, &main_loop_cap);
}
