use super::{BoxError, CounterProvider, InterfaceCounter, InterfaceId};
use std::{
    ffi::CStr,
    io,
    mem::{self, MaybeUninit},
    ptr,
};

pub(super) struct MacOsCounterProvider;

impl MacOsCounterProvider {
    pub(super) fn new() -> Self {
        Self
    }
}

impl CounterProvider for MacOsCounterProvider {
    fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError> {
        let list = InterfaceList::read()?;
        let mut counters = Vec::new();
        let mut current = list.0;

        while !current.is_null() {
            let entry = unsafe { &*current };
            if entry.if_index == 0 || entry.if_name.is_null() {
                break;
            }
            let index = entry.if_index;
            let name = unsafe { CStr::from_ptr(entry.if_name) }
                .to_bytes()
                .to_vec()
                .into_boxed_slice();
            if let Some(data) = read_interface_data(index)?
                && let Some(counter) = counter_from_data(index, name, &data)
            {
                counters.push(counter);
            }
            current = unsafe { current.add(1) };
        }

        Ok(counters)
    }
}

fn counter_from_data(
    index: u32,
    name: Box<[u8]>,
    data: &libc::ifmibdata,
) -> Option<InterfaceCounter> {
    if data.ifmd_flags & libc::IFF_UP as u32 == 0 {
        return None;
    }

    Some(InterfaceCounter {
        id: InterfaceId::with_incarnation(u64::from(index), name),
        received_bytes: data.ifmd_data.ifi_ibytes,
        loopback: data.ifmd_flags & libc::IFF_LOOPBACK as u32 != 0,
    })
}

struct InterfaceList(*mut libc::if_nameindex);

impl InterfaceList {
    fn read() -> io::Result<Self> {
        let list = unsafe { libc::if_nameindex() };
        if list.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(list))
        }
    }
}

impl Drop for InterfaceList {
    fn drop(&mut self) {
        unsafe {
            libc::if_freenameindex(self.0);
        }
    }
}

fn read_interface_data(index: u32) -> io::Result<Option<libc::ifmibdata>> {
    let index = i32::try_from(index)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "interface index overflow"))?;
    let mut mib = [
        libc::CTL_NET,
        libc::PF_LINK,
        libc::NETLINK_GENERIC,
        libc::IFMIB_IFDATA,
        index,
        libc::IFDATA_GENERAL,
    ];
    let mut data = MaybeUninit::<libc::ifmibdata>::zeroed();
    let mut length = mem::size_of::<libc::ifmibdata>();
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            data.as_mut_ptr().cast(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == libc::ENOENT || code == libc::ENXIO => Ok(None),
            _ => Err(error),
        };
    }
    if length != mem::size_of::<libc::ifmibdata>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected interface statistics size",
        ));
    }

    Ok(Some(unsafe { data.assume_init() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_activity::CounterProvider;

    #[test]
    fn only_operational_interfaces_are_mapped() {
        let mut data = unsafe { mem::zeroed::<libc::ifmibdata>() };
        data.ifmd_data.ifi_ibytes = 4_200;

        assert_eq!(
            counter_from_data(7, b"en0".to_vec().into_boxed_slice(), &data),
            None
        );

        data.ifmd_flags = libc::IFF_UP as u32;
        let counter = counter_from_data(7, b"en0".to_vec().into_boxed_slice(), &data)
            .expect("up interface should be mapped");
        assert_eq!(
            counter.id,
            InterfaceId::with_incarnation(7, b"en0".as_slice().into())
        );
        assert_eq!(counter.received_bytes, 4_200);
        assert!(!counter.loopback);
    }

    #[test]
    fn native_provider_reads_sane_interface_counters() {
        let counters = MacOsCounterProvider::new()
            .read()
            .expect("macOS interface counters should be readable");

        assert!(
            !counters.is_empty(),
            "macOS should expose at least loopback"
        );
        assert!(counters.iter().all(|counter| counter.id.numeric > 0));
    }
}
