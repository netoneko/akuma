// ARM Generic Interrupt Controller (GIC) v2 driver
// For QEMU ARM virt machine

// ============================================================================
// GIC Register Offsets
// ============================================================================

/// GIC Distributor register offsets
mod dist {
    pub const CTLR: usize = 0x000; // Control Register
    pub const ISENABLER: usize = 0x100; // Interrupt Set-Enable Registers
    pub const ICENABLER: usize = 0x180; // Interrupt Clear-Enable Registers
    pub const IPRIORITYR: usize = 0x400; // Interrupt Priority Registers
    pub const ITARGETSR: usize = 0x800; // Interrupt Processor Targets
    pub const SGIR: usize = 0xF00; // Software Generated Interrupt Register
}

/// GIC CPU Interface register offsets
mod cpu {
    pub const CTLR: usize = 0x000; // CPU Interface Control Register
    pub const PMR: usize = 0x004; // Interrupt Priority Mask Register
    pub const IAR: usize = 0x00C; // Interrupt Acknowledge Register
    pub const EOIR: usize = 0x010; // End of Interrupt Register
}

// ============================================================================
// GIC Driver - Encapsulates all MMIO access
// ============================================================================

/// ARM GIC v2 driver that encapsulates all MMIO access
struct Gic {
    dist_base: usize,
    cpu_base: usize,
}

impl Gic {
    /// Create a new GIC driver with the given base addresses
    const fn new(dist_base: usize, cpu_base: usize) -> Self {
        Self {
            dist_base,
            cpu_base,
        }
    }

    // ========================================================================
    // Low-level register access (all unsafe operations localized here)
    // ========================================================================

    /// Write to a distributor register
    #[inline]
    fn write_dist(&self, offset: usize, value: u32) {
        // SAFETY: Writing to GIC distributor register at known QEMU virt machine address
        unsafe {
            core::ptr::write_volatile((self.dist_base + offset) as *mut u32, value);
        }
    }

    /// Read from a distributor register
    #[inline]
    fn read_dist(&self, offset: usize) -> u32 {
        // SAFETY: Reading from GIC distributor register at known QEMU virt machine address
        unsafe { core::ptr::read_volatile((self.dist_base + offset) as *const u32) }
    }

    /// Write a byte to a distributor register
    #[inline]
    fn write_dist_byte(&self, offset: usize, value: u8) {
        // SAFETY: Writing to GIC distributor register at known address
        unsafe {
            core::ptr::write_volatile((self.dist_base + offset) as *mut u8, value);
        }
    }

    /// Write to a CPU interface register
    #[inline]
    fn write_cpu(&self, offset: usize, value: u32) {
        // SAFETY: Writing to GIC CPU interface register at known QEMU virt machine address
        unsafe {
            core::ptr::write_volatile((self.cpu_base + offset) as *mut u32, value);
        }
    }

    /// Read from a CPU interface register
    #[inline]
    fn read_cpu(&self, offset: usize) -> u32 {
        // SAFETY: Reading from GIC CPU interface register at known QEMU virt machine address
        unsafe { core::ptr::read_volatile((self.cpu_base + offset) as *const u32) }
    }

    // ========================================================================
    // High-level GIC operations (safe wrappers)
    // ========================================================================

    /// Initialize the GIC
    fn init(&self) {
        // Disable distributor
        self.write_dist(dist::CTLR, 0);

        // Disable all interrupts
        for i in 0..32 {
            self.write_dist(dist::ICENABLER + i * 4, 0xFFFF_FFFF);
        }

        // Set all interrupts to lowest priority
        for i in 0..256 {
            self.write_dist(dist::IPRIORITYR + i * 4, 0xA0A0_A0A0);
        }

        // Route all interrupts to CPU 0
        for i in 8..256 {
            self.write_dist(dist::ITARGETSR + i * 4, 0x0101_0101);
        }

        // Enable distributor
        self.write_dist(dist::CTLR, 1);

        // Configure CPU interface
        // Set priority mask to allow all interrupts
        self.write_cpu(cpu::PMR, 0xFF);

        // Enable CPU interface
        self.write_cpu(cpu::CTLR, 1);
    }

    /// Enable a specific IRQ
    fn enable_irq(&self, irq: u32) {
        if irq >= 1020 {
            return; // Invalid IRQ number
        }

        let offset = dist::ISENABLER + ((irq / 32) * 4) as usize;
        let bit = 1u32 << (irq % 32);
        self.write_dist(offset, bit);
    }

    /// Disable a specific IRQ
    fn disable_irq(&self, irq: u32) {
        if irq >= 1020 {
            return; // Invalid IRQ number
        }

        let offset = dist::ICENABLER + ((irq / 32) * 4) as usize;
        let bit = 1u32 << (irq % 32);
        self.write_dist(offset, bit);
    }

    /// Acknowledge an interrupt and return its IRQ number
    fn acknowledge_irq(&self) -> Option<u32> {
        let iar = self.read_cpu(cpu::IAR);
        let irq = iar & 0x3FF;

        // IRQ 1023 is a spurious interrupt
        if irq >= 1020 { None } else { Some(irq) }
    }

    /// Signal end of interrupt handling
    fn end_of_interrupt(&self, irq: u32) {
        self.write_cpu(cpu::EOIR, irq);
    }

    /// Trigger a Software Generated Interrupt (SGI)
    fn trigger_sgi(&self, sgi_id: u32) {
        if sgi_id > 15 {
            return; // Invalid SGI ID
        }

        // GICD_SGIR format:
        // [25:24] = TargetListFilter (0b10 = send to requesting CPU only)
        // [23:16] = CPUTargetList (ignored when filter=0b10)
        // [15] = NSATT (0 = secure)
        // [3:0] = SGIINTID (SGI number 0-15)
        let value = (0b10 << 24) | sgi_id;
        self.write_dist(dist::SGIR, value);
    }

    /// Set interrupt priority (0 = highest, 255 = lowest)
    fn set_priority(&self, irq: u32, priority: u8) {
        if irq >= 1020 {
            return;
        }

        let offset = dist::IPRIORITYR + irq as usize;
        self.write_dist_byte(offset, priority);
    }
}

/// Global GIC instance for QEMU virt machine
static GIC: Gic = Gic::new(0x0800_0000, 0x0801_0000);

// ============================================================================
// Public API - Safe wrappers around GIC operations
// ============================================================================

/// SGI numbers (0-15)
pub const SGI_SCHEDULER: u32 = 0; // SGI 0 for scheduling

/// Initialize the GIC
pub fn init() {
    GIC.init();
}

/// Enable a specific IRQ
pub fn enable_irq(irq: u32) {
    GIC.enable_irq(irq);
}

/// Disable a specific IRQ
pub fn disable_irq(irq: u32) {
    GIC.disable_irq(irq);
}

/// Acknowledge an interrupt and return its IRQ number
pub fn acknowledge_irq() -> Option<u32> {
    GIC.acknowledge_irq()
}

/// Signal end of interrupt handling
pub fn end_of_interrupt(irq: u32) {
    GIC.end_of_interrupt(irq);
}

/// Trigger a Software Generated Interrupt (SGI)
///
/// SGI 0-15 are available. This sends the interrupt to the current CPU.
pub fn trigger_sgi(sgi_id: u32) {
    GIC.trigger_sgi(sgi_id);
}

/// Set interrupt priority (0 = highest, 255 = lowest)
#[allow(dead_code)]
pub fn set_priority(irq: u32, priority: u8) {
    GIC.set_priority(irq, priority);
}
