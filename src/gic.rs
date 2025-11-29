// ARM Generic Interrupt Controller (GIC) v2 driver
// For QEMU ARM virt machine

use core::ptr::{read_volatile, write_volatile};

// GIC distributor base address for QEMU virt machine
const GICD_BASE: usize = 0x0800_0000;
// GIC CPU interface base address
const GICC_BASE: usize = 0x0801_0000;

// GIC Distributor registers
const GICD_CTLR: usize = GICD_BASE + 0x000; // Control Register
const GICD_ISENABLER: usize = GICD_BASE + 0x100; // Interrupt Set-Enable Registers
const GICD_ICENABLER: usize = GICD_BASE + 0x180; // Interrupt Clear-Enable Registers
const GICD_IPRIORITYR: usize = GICD_BASE + 0x400; // Interrupt Priority Registers
const GICD_ITARGETSR: usize = GICD_BASE + 0x800; // Interrupt Processor Targets

// GIC CPU Interface registers
const GICC_CTLR: usize = GICC_BASE + 0x000; // CPU Interface Control Register
const GICC_PMR: usize = GICC_BASE + 0x004; // Interrupt Priority Mask Register
const GICC_IAR: usize = GICC_BASE + 0x00C; // Interrupt Acknowledge Register
const GICC_EOIR: usize = GICC_BASE + 0x010; // End of Interrupt Register

/// Initialize the GIC
pub fn init() {
    unsafe {
        // Disable distributor
        write_volatile(GICD_CTLR as *mut u32, 0);

        // Disable all interrupts
        for i in 0..32 {
            write_volatile((GICD_ICENABLER + i * 4) as *mut u32, 0xFFFF_FFFF);
        }

        // Set all interrupts to lowest priority
        for i in 0..256 {
            write_volatile((GICD_IPRIORITYR + i * 4) as *mut u32, 0xA0A0_A0A0);
        }

        // Route all interrupts to CPU 0
        for i in 8..256 {
            write_volatile((GICD_ITARGETSR + i * 4) as *mut u32, 0x0101_0101);
        }

        // Enable distributor
        write_volatile(GICD_CTLR as *mut u32, 1);

        // Configure CPU interface
        // Set priority mask to allow all interrupts
        write_volatile(GICC_PMR as *mut u32, 0xFF);

        // Enable CPU interface
        write_volatile(GICC_CTLR as *mut u32, 1);
    }
}

/// Enable a specific IRQ
pub fn enable_irq(irq: u32) {
    if irq >= 1020 {
        return; // Invalid IRQ number
    }

    unsafe {
        let reg = (GICD_ISENABLER + ((irq / 32) * 4) as usize) as *mut u32;
        let bit = 1u32 << (irq % 32);
        write_volatile(reg, bit);
    }
}

/// Disable a specific IRQ
pub fn disable_irq(irq: u32) {
    if irq >= 1020 {
        return; // Invalid IRQ number
    }

    unsafe {
        let reg = (GICD_ICENABLER + ((irq / 32) * 4) as usize) as *mut u32;
        let bit = 1u32 << (irq % 32);
        write_volatile(reg, bit);
    }
}

/// Acknowledge an interrupt and return its IRQ number
pub fn acknowledge_irq() -> Option<u32> {
    unsafe {
        let iar = read_volatile(GICC_IAR as *const u32);
        let irq = iar & 0x3FF;

        // IRQ 1023 is a spurious interrupt
        if irq >= 1020 { None } else { Some(irq) }
    }
}

/// Signal end of interrupt handling
pub fn end_of_interrupt(irq: u32) {
    unsafe {
        write_volatile(GICC_EOIR as *mut u32, irq);
    }
}

/// Set interrupt priority (0 = highest, 255 = lowest)
pub fn set_priority(irq: u32, priority: u8) {
    if irq >= 1020 {
        return;
    }

    unsafe {
        let reg = (GICD_IPRIORITYR + irq as usize) as *mut u8;
        write_volatile(reg, priority);
    }
}
