use super::{BoxError, CounterProvider, InterfaceCounter, InterfaceId};
use std::{io, ptr, slice};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;

pub(super) struct WindowsCounterProvider;

impl WindowsCounterProvider {
    pub(super) fn new() -> Self {
        Self
    }
}

impl CounterProvider for WindowsCounterProvider {
    fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError> {
        let table = InterfaceTable::read()?;
        let rows = table.rows()?;
        Ok(rows.iter().filter_map(counter_from_row).collect())
    }
}

fn counter_from_row(row: &MIB_IF_ROW2) -> Option<InterfaceCounter> {
    if row.OperStatus != IfOperStatusUp {
        return None;
    }

    Some(InterfaceCounter {
        id: InterfaceId::numeric(unsafe { row.InterfaceLuid.Value }),
        received_bytes: row.InOctets,
        loopback: row.Type == IF_TYPE_SOFTWARE_LOOPBACK,
    })
}

struct InterfaceTable(*mut MIB_IF_TABLE2);

impl InterfaceTable {
    fn read() -> io::Result<Self> {
        let mut table = ptr::null_mut();
        let status = unsafe { GetIfTable2(&mut table) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        if table.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "GetIfTable2 returned a null table",
            ));
        }
        Ok(Self(table))
    }

    fn rows(&self) -> io::Result<&[MIB_IF_ROW2]> {
        let count = unsafe { (*self.0).NumEntries };
        let count = usize::try_from(count)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "interface count overflow"))?;
        Ok(unsafe { slice::from_raw_parts((*self.0).Table.as_ptr(), count) })
    }
}

impl Drop for InterfaceTable {
    fn drop(&mut self) {
        unsafe {
            FreeMibTable(self.0.cast());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::NetworkManagement::Ndis::{IfOperStatusDown, IfOperStatusUp};

    #[test]
    fn only_operational_interfaces_are_mapped() {
        let mut row = unsafe { std::mem::zeroed::<MIB_IF_ROW2>() };
        row.OperStatus = IfOperStatusDown;
        assert_eq!(counter_from_row(&row), None);

        row.OperStatus = IfOperStatusUp;
        row.InOctets = 4_200;
        row.InterfaceLuid.Value = 7;
        let counter = counter_from_row(&row).expect("up interface should be mapped");
        assert_eq!(counter.id, InterfaceId::numeric(7));
        assert_eq!(counter.received_bytes, 4_200);
        assert!(!counter.loopback);
    }

    #[test]
    fn native_provider_reads_sane_interface_counters() {
        let counters = WindowsCounterProvider::new()
            .read()
            .expect("Windows interface counters should be readable");

        assert!(
            !counters.is_empty(),
            "Windows should expose at least loopback"
        );
        assert!(counters.iter().all(|counter| counter.id.numeric > 0));
    }
}
