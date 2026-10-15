use thiserror::Error;

pub const DEVICE_ID: u8 = 0x00;
pub const DEVICE_VENDOR_MASK: u8 = 0xf0;
pub const MAXIM_VENDOR_ID: u8 = 0x20;
pub const STATUS1: u8 = 0x03;
pub const CONTROL1: u8 = 0x07;
pub const CONTROL2: u8 = 0x08;
pub const CONTROL3: u8 = 0x09;
pub const DATA_SWITCH_MASK: u8 = 0x0c;
pub const USB_DATA_PATH: u8 = 0x08;
pub const VB_VALID_MASK: u8 = 0x10;
pub const CONTROL1_POWER_ON_DEFAULT: u8 = 0x0d;
pub const CONTROL2_POWER_ON_DEFAULT: u8 = 0x8e;
pub const CONTROL3_POWER_ON_DEFAULT: u8 = 0x8d;

const CE_FORCE_MASK: u8 = 0x60;
const CE_FORCE_HIGH_IMPEDANCE: u8 = 0x40;
const ID_AUTO_SWITCH_MASK: u8 = 0x02;
const OVP_CONTROL_MASK: u8 = 0xc0;
const OVP_FORCE_CLOSED: u8 = 0x40;
const CHARGER_DETECTION_MASK: u8 = 0x07;
const SNAPSHOT_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Max20332Status {
    pub device_id: u8,
    pub status1: u8,
    pub control1: u8,
    pub control2: u8,
    pub control3: u8,
}

impl Max20332Status {
    pub const fn vbus_valid(self) -> bool {
        self.status1 & VB_VALID_MASK != 0
    }

    pub const fn data_routed_to_usb(self) -> bool {
        self.control1 & DATA_SWITCH_MASK == USB_DATA_PATH
    }

    pub const fn charger_detection_disabled(self) -> bool {
        self.control3 & CHARGER_DETECTION_MASK == 0
    }

    pub const fn ovp_forced_closed(self) -> bool {
        self.control3 & OVP_CONTROL_MASK == OVP_FORCE_CLOSED
    }

    pub const fn ce_forced_high_impedance(self) -> bool {
        self.control2 & CE_FORCE_MASK == CE_FORCE_HIGH_IMPEDANCE
    }

    pub const fn id_auto_switch_disabled(self) -> bool {
        self.control2 & ID_AUTO_SWITCH_MASK == 0
    }

    pub const fn host_electrical_configured(self) -> bool {
        self.data_routed_to_usb()
            && self.charger_detection_disabled()
            && self.ovp_forced_closed()
            && self.ce_forced_high_impedance()
            && self.id_auto_switch_disabled()
    }

    pub const fn register_snapshot(self) -> Max20332RegisterSnapshot {
        Max20332RegisterSnapshot {
            device_id: self.device_id,
            control1: self.control1,
            control2: self.control2,
            control3: self.control3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Max20332RegisterSnapshot {
    pub device_id: u8,
    pub control1: u8,
    pub control2: u8,
    pub control3: u8,
}

impl Max20332RegisterSnapshot {
    pub const fn target_power_on_defaults(device_id: u8) -> Self {
        Self {
            device_id,
            control1: CONTROL1_POWER_ON_DEFAULT,
            control2: CONTROL2_POWER_ON_DEFAULT,
            control3: CONTROL3_POWER_ON_DEFAULT,
        }
    }

    pub const fn registers_are_target_power_on_defaults(self) -> bool {
        self.control1 == CONTROL1_POWER_ON_DEFAULT
            && self.control2 == CONTROL2_POWER_ON_DEFAULT
            && self.control3 == CONTROL3_POWER_ON_DEFAULT
    }

    pub const fn host_configuration(self) -> Self {
        Self {
            device_id: self.device_id,
            control1: (self.control1 & !DATA_SWITCH_MASK) | USB_DATA_PATH,
            control2: (self.control2 & !(CE_FORCE_MASK | ID_AUTO_SWITCH_MASK))
                | CE_FORCE_HIGH_IMPEDANCE,
            control3: (self.control3 & !(OVP_CONTROL_MASK | CHARGER_DETECTION_MASK))
                | OVP_FORCE_CLOSED,
        }
    }

    pub fn encode(self) -> Vec<u8> {
        format!(
            "version={SNAPSHOT_VERSION}\ndevice_id={:02x}\ncontrol1={:02x}\ncontrol2={:02x}\ncontrol3={:02x}\n",
            self.device_id, self.control1, self.control2, self.control3
        )
        .into_bytes()
    }

    pub fn decode(contents: &[u8]) -> Result<Self, Max20332Error> {
        let text = std::str::from_utf8(contents).map_err(|_| Max20332Error::InvalidSnapshot)?;
        let mut lines = text.lines();
        if lines.next() != Some("version=1") {
            return Err(Max20332Error::InvalidSnapshot);
        }
        let snapshot = Self {
            device_id: parse_snapshot_byte(lines.next(), "device_id=")?,
            control1: parse_snapshot_byte(lines.next(), "control1=")?,
            control2: parse_snapshot_byte(lines.next(), "control2=")?,
            control3: parse_snapshot_byte(lines.next(), "control3=")?,
        };
        if lines.next().is_some() || snapshot.encode() != contents {
            return Err(Max20332Error::InvalidSnapshot);
        }
        if snapshot.device_id & DEVICE_VENDOR_MASK != MAXIM_VENDOR_ID {
            return Err(Max20332Error::InvalidSnapshot);
        }
        Ok(snapshot)
    }
}

fn parse_snapshot_byte(line: Option<&str>, prefix: &str) -> Result<u8, Max20332Error> {
    let value = line
        .and_then(|line| line.strip_prefix(prefix))
        .filter(|value| value.len() == 2)
        .ok_or(Max20332Error::InvalidSnapshot)?;
    u8::from_str_radix(value, 16).map_err(|_| Max20332Error::InvalidSnapshot)
}

#[derive(Debug, Error)]
pub enum Max20332Error {
    #[error("MAX20332 register I/O failed: {0}")]
    Io(String),
    #[error("MAX20332 returned an invalid device identifier: {0:#04x}")]
    InvalidDeviceId(u8),
    #[error(
        "MAX20332 register {register:#04x} verification failed: wrote {expected:#04x}, read {actual:#04x}"
    )]
    Verification {
        register: u8,
        expected: u8,
        actual: u8,
    },
    #[error("MAX20332 persisted register snapshot is malformed or unsupported")]
    InvalidSnapshot,
    #[error(
        "MAX20332 live register state does not match the persisted original snapshot for this device"
    )]
    SnapshotMismatch,
    #[error(
        "MAX20332 host configuration failed ({operation}); exact register rollback also failed ({rollback})"
    )]
    RollbackFailed { operation: String, rollback: String },
}

pub trait RegisterBus: std::fmt::Debug {
    fn read_byte(&mut self, register: u8) -> Result<u8, Max20332Error>;
    fn write_byte(&mut self, register: u8, value: u8) -> Result<(), Max20332Error>;
}

#[derive(Debug)]
pub struct Max20332<B> {
    bus: B,
}

impl<B: RegisterBus> Max20332<B> {
    pub const fn new(bus: B) -> Self {
        Self { bus }
    }

    pub fn validate(&mut self) -> Result<u8, Max20332Error> {
        let id = self.bus.read_byte(DEVICE_ID)?;
        if id & DEVICE_VENDOR_MASK != MAXIM_VENDOR_ID {
            return Err(Max20332Error::InvalidDeviceId(id));
        }
        Ok(id)
    }

    pub fn route_data_to_usb(&mut self) -> Result<u8, Max20332Error> {
        self.validate()?;
        let previous = self.bus.read_byte(CONTROL1)?;
        let requested = (previous & !DATA_SWITCH_MASK) | USB_DATA_PATH;
        if requested != previous {
            self.bus.write_byte(CONTROL1, requested)?;
        }
        let actual = self.bus.read_byte(CONTROL1)?;
        if actual != requested {
            return Err(Max20332Error::Verification {
                register: CONTROL1,
                expected: requested,
                actual,
            });
        }
        Ok(actual)
    }

    pub fn register_snapshot(&mut self) -> Result<Max20332RegisterSnapshot, Max20332Error> {
        Ok(Max20332RegisterSnapshot {
            device_id: self.validate()?,
            control1: self.bus.read_byte(CONTROL1)?,
            control2: self.bus.read_byte(CONTROL2)?,
            control3: self.bus.read_byte(CONTROL3)?,
        })
    }

    pub fn configure_host(
        &mut self,
        original: Max20332RegisterSnapshot,
    ) -> Result<Max20332Status, Max20332Error> {
        if self.register_snapshot()? != original {
            return Err(Max20332Error::SnapshotMismatch);
        }
        let requested = original.host_configuration();
        let operation = self
            .write_verified(CONTROL3, requested.control3)
            .and_then(|()| self.write_verified(CONTROL2, requested.control2))
            .and_then(|()| self.write_verified(CONTROL1, requested.control1));
        if let Err(operation) = operation {
            return self.rollback_configuration(original, operation);
        }
        match self.inspect() {
            Ok(status) if status.host_electrical_configured() => Ok(status),
            Ok(_) => self.rollback_configuration(original, Max20332Error::SnapshotMismatch),
            Err(operation) => self.rollback_configuration(original, operation),
        }
    }

    pub fn restore(&mut self, original: Max20332RegisterSnapshot) -> Result<(), Max20332Error> {
        let actual_device = self.validate()?;
        if actual_device != original.device_id {
            return Err(Max20332Error::SnapshotMismatch);
        }
        let mut failure = None;
        for (register, value) in [
            (CONTROL1, original.control1),
            (CONTROL2, original.control2),
            (CONTROL3, original.control3),
        ] {
            if let Err(error) = self.write_verified(register, value) {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if self.register_snapshot()? != original {
            return Err(Max20332Error::SnapshotMismatch);
        }
        Ok(())
    }

    pub fn inspect(&mut self) -> Result<Max20332Status, Max20332Error> {
        Ok(Max20332Status {
            device_id: self.validate()?,
            status1: self.bus.read_byte(STATUS1)?,
            control1: self.bus.read_byte(CONTROL1)?,
            control2: self.bus.read_byte(CONTROL2)?,
            control3: self.bus.read_byte(CONTROL3)?,
        })
    }

    fn write_verified(&mut self, register: u8, expected: u8) -> Result<(), Max20332Error> {
        if self.bus.read_byte(register)? != expected {
            self.bus.write_byte(register, expected)?;
        }
        let actual = self.bus.read_byte(register)?;
        if actual != expected {
            return Err(Max20332Error::Verification {
                register,
                expected,
                actual,
            });
        }
        Ok(())
    }

    fn rollback_configuration<T>(
        &mut self,
        original: Max20332RegisterSnapshot,
        operation: Max20332Error,
    ) -> Result<T, Max20332Error> {
        match self.restore(original) {
            Ok(()) => Err(operation),
            Err(rollback) => Err(Max20332Error::RollbackFailed {
                operation: operation.to_string(),
                rollback: rollback.to_string(),
            }),
        }
    }

    pub fn into_inner(self) -> B {
        self.bus
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct LinuxRegisterBus {
    device: i2cdev::linux::LinuxI2CDevice,
}

#[cfg(target_os = "linux")]
impl LinuxRegisterBus {
    pub fn open(path: &std::path::Path, address: u16) -> Result<Self, Max20332Error> {
        use i2cdev::linux::LinuxI2CDevice;

        LinuxI2CDevice::new(path, address)
            .map(|device| Self { device })
            .map_err(|error| Max20332Error::Io(error.to_string()))
    }
}

#[cfg(target_os = "linux")]
impl RegisterBus for LinuxRegisterBus {
    fn read_byte(&mut self, register: u8) -> Result<u8, Max20332Error> {
        use i2cdev::core::I2CDevice;

        self.device
            .smbus_read_byte_data(register)
            .map_err(|error| Max20332Error::Io(error.to_string()))
    }

    fn write_byte(&mut self, register: u8, value: u8) -> Result<(), Max20332Error> {
        use i2cdev::core::I2CDevice;

        self.device
            .smbus_write_byte_data(register, value)
            .map_err(|error| Max20332Error::Io(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[derive(Debug)]
    struct FakeBus {
        registers: BTreeMap<u8, u8>,
        writes: Vec<(u8, u8)>,
        read_attempts: usize,
        write_attempts: usize,
        fail_read_attempts: BTreeSet<usize>,
        fail_write_attempts: BTreeSet<usize>,
    }

    impl FakeBus {
        fn connected(control1: u8) -> Self {
            Self {
                registers: BTreeMap::from([
                    (DEVICE_ID, 0x2f),
                    (STATUS1, VB_VALID_MASK | 0x02),
                    (CONTROL1, control1),
                    (CONTROL2, CONTROL2_POWER_ON_DEFAULT),
                    (CONTROL3, CONTROL3_POWER_ON_DEFAULT),
                ]),
                writes: Vec::new(),
                read_attempts: 0,
                write_attempts: 0,
                fail_read_attempts: BTreeSet::new(),
                fail_write_attempts: BTreeSet::new(),
            }
        }

        fn snapshot(&self) -> Max20332RegisterSnapshot {
            Max20332RegisterSnapshot {
                device_id: self.registers[&DEVICE_ID],
                control1: self.registers[&CONTROL1],
                control2: self.registers[&CONTROL2],
                control3: self.registers[&CONTROL3],
            }
        }
    }

    impl RegisterBus for FakeBus {
        fn read_byte(&mut self, register: u8) -> Result<u8, Max20332Error> {
            let attempt = self.read_attempts;
            self.read_attempts += 1;
            if self.fail_read_attempts.remove(&attempt) {
                return Err(Max20332Error::Io(format!(
                    "injected read failure at attempt {attempt}"
                )));
            }
            self.registers
                .get(&register)
                .copied()
                .ok_or_else(|| Max20332Error::Io(format!("missing register {register:#04x}")))
        }

        fn write_byte(&mut self, register: u8, value: u8) -> Result<(), Max20332Error> {
            let attempt = self.write_attempts;
            self.write_attempts += 1;
            if self.fail_write_attempts.remove(&attempt) {
                return Err(Max20332Error::Io(format!(
                    "injected write failure at attempt {attempt}"
                )));
            }
            self.registers.insert(register, value);
            self.writes.push((register, value));
            Ok(())
        }
    }

    #[test]
    fn routes_usb_without_clobbering_unrelated_bits() {
        let mut switch = Max20332::new(FakeBus::connected(0xa5));
        let value = switch.route_data_to_usb().expect("switch should succeed");
        let bus = switch.into_inner();

        assert_eq!(value, 0xa9);
        assert_eq!(bus.writes, vec![(CONTROL1, 0xa9)]);
    }

    #[test]
    fn does_not_write_when_already_routed() {
        let mut switch = Max20332::new(FakeBus::connected(0x9b));
        switch.route_data_to_usb().expect("switch should succeed");

        assert!(switch.into_inner().writes.is_empty());
    }

    #[test]
    fn rejects_open_bus_values() {
        for id in [0x00, 0x42, 0xff] {
            let mut bus = FakeBus::connected(0);
            bus.registers.insert(DEVICE_ID, id);
            let error = Max20332::new(bus)
                .validate()
                .expect_err("open bus value must be rejected");
            assert!(matches!(error, Max20332Error::InvalidDeviceId(value) if value == id));
        }
    }

    #[test]
    fn reports_vbus_and_switch_state_without_writing() {
        let mut bus = FakeBus::connected(USB_DATA_PATH);
        bus.registers.insert(STATUS1, VB_VALID_MASK | 0x06);
        let mut switch = Max20332::new(bus);

        let status = switch.inspect().expect("inspection should succeed");

        assert!(status.vbus_valid());
        assert!(status.data_routed_to_usb());
        assert!(switch.into_inner().writes.is_empty());
    }

    #[test]
    fn snapshot_encoding_is_canonical_and_strict() {
        let snapshot = Max20332RegisterSnapshot::target_power_on_defaults(0x2f);
        let encoded = snapshot.encode();
        assert_eq!(
            encoded,
            b"version=1\ndevice_id=2f\ncontrol1=0d\ncontrol2=8e\ncontrol3=8d\n"
        );
        assert_eq!(
            Max20332RegisterSnapshot::decode(&encoded).expect("decode"),
            snapshot
        );
        for malformed in [
            b"version=2\ndevice_id=2f\ncontrol1=0d\ncontrol2=8e\ncontrol3=8d\n".as_slice(),
            b"version=1\ndevice_id=ff\ncontrol1=0d\ncontrol2=8e\ncontrol3=8d\n".as_slice(),
            b"version=1\ndevice_id=2F\ncontrol1=0d\ncontrol2=8e\ncontrol3=8d\n".as_slice(),
            b"version=1\ndevice_id=2f\ncontrol1=0d\ncontrol2=8e\ncontrol3=8d".as_slice(),
        ] {
            assert!(matches!(
                Max20332RegisterSnapshot::decode(malformed),
                Err(Max20332Error::InvalidSnapshot)
            ));
        }
    }

    #[test]
    fn host_configuration_preserves_unrelated_bits_and_uses_safe_order() {
        let mut bus = FakeBus::connected(0xa5);
        bus.registers.insert(CONTROL2, 0x9f);
        bus.registers.insert(CONTROL3, 0xbe);
        let original = bus.snapshot();
        let requested = original.host_configuration();
        let mut switch = Max20332::new(bus);

        let status = switch.configure_host(original).expect("host policy");
        let bus = switch.into_inner();

        assert_eq!(requested.control1, 0xa9);
        assert_eq!(requested.control2, 0xdd);
        assert_eq!(requested.control3, 0x78);
        assert_eq!(status.register_snapshot(), requested);
        assert!(status.host_electrical_configured());
        assert_eq!(
            bus.writes,
            vec![
                (CONTROL3, requested.control3),
                (CONTROL2, requested.control2),
                (CONTROL1, requested.control1),
            ]
        );
    }

    #[test]
    fn target_defaults_produce_verified_live_host_values() {
        let bus = FakeBus::connected(CONTROL1_POWER_ON_DEFAULT);
        let original = bus.snapshot();
        assert!(original.registers_are_target_power_on_defaults());
        let mut switch = Max20332::new(bus);

        let status = switch.configure_host(original).expect("host policy");

        assert_eq!(status.control1, 0x09);
        assert_eq!(status.control2, 0xcc);
        assert_eq!(status.control3, 0x48);
    }

    #[test]
    fn mid_sequence_failure_rolls_back_exact_original() {
        let mut bus = FakeBus::connected(CONTROL1_POWER_ON_DEFAULT);
        let original = bus.snapshot();
        bus.fail_write_attempts.insert(1);
        let mut switch = Max20332::new(bus);

        let error = switch
            .configure_host(original)
            .expect_err("second write must fail");
        let bus = switch.into_inner();

        assert!(matches!(error, Max20332Error::Io(_)));
        assert_eq!(bus.snapshot(), original);
        assert_eq!(
            bus.writes,
            vec![(CONTROL3, 0x48), (CONTROL3, original.control3)]
        );
    }

    #[test]
    fn rollback_failure_is_never_hidden() {
        let mut bus = FakeBus::connected(CONTROL1_POWER_ON_DEFAULT);
        let original = bus.snapshot();
        bus.fail_write_attempts.extend([1, 2]);
        let mut switch = Max20332::new(bus);

        let error = switch
            .configure_host(original)
            .expect_err("operation and rollback must fail");

        assert!(matches!(error, Max20332Error::RollbackFailed { .. }));
    }

    #[test]
    fn final_inspection_failure_rolls_back_exact_original() {
        let mut bus = FakeBus::connected(CONTROL1_POWER_ON_DEFAULT);
        let original = bus.snapshot();
        bus.fail_read_attempts.insert(10);
        let mut switch = Max20332::new(bus);

        let error = switch
            .configure_host(original)
            .expect_err("final inspection must fail");
        let bus = switch.into_inner();

        assert!(matches!(error, Max20332Error::Io(_)));
        assert_eq!(bus.snapshot(), original);
    }

    #[test]
    fn restore_writes_all_exact_registers_in_device_safe_order() {
        let original = Max20332RegisterSnapshot::target_power_on_defaults(0x2f);
        let mut bus = FakeBus::connected(0x09);
        bus.registers.insert(CONTROL2, 0xcc);
        bus.registers.insert(CONTROL3, 0x48);
        let mut switch = Max20332::new(bus);

        switch.restore(original).expect("exact restore");
        let bus = switch.into_inner();

        assert_eq!(bus.snapshot(), original);
        assert_eq!(
            bus.writes,
            vec![(CONTROL1, 0x0d), (CONTROL2, 0x8e), (CONTROL3, 0x8d)]
        );
    }

    #[test]
    fn restore_attempts_later_registers_after_a_fault() {
        let original = Max20332RegisterSnapshot::target_power_on_defaults(0x2f);
        let mut bus = FakeBus::connected(0x09);
        bus.registers.insert(CONTROL2, 0xcc);
        bus.registers.insert(CONTROL3, 0x48);
        bus.fail_write_attempts.insert(0);
        let mut switch = Max20332::new(bus);

        switch
            .restore(original)
            .expect_err("first restore write fails");
        let bus = switch.into_inner();

        assert_eq!(bus.registers[&CONTROL1], 0x09);
        assert_eq!(bus.registers[&CONTROL2], 0x8e);
        assert_eq!(bus.registers[&CONTROL3], 0x8d);
        assert_eq!(bus.writes, vec![(CONTROL2, 0x8e), (CONTROL3, 0x8d)]);
    }
}
