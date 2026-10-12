//! PCI configuration reads on segment 0 through EFI_PCI_ROOT_BRIDGE_IO_PROTOCOL, which reaches extended registers.

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, SearchType};
use uefi::proto::pci::PciIoAddress;
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

pub struct Config(ScopedProtocol<PciRootBridgeIo>);

impl Config {
    /// The segment 0 root bridge that answers at 00:00.0; None without one.
    pub fn open() -> Option<Config> {
        let handles = boot::locate_handle_buffer(SearchType::from_proto::<PciRootBridgeIo>()).ok()?;
        handles.iter().find_map(|&handle| {
            let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
            let p =
                unsafe { boot::open_protocol::<PciRootBridgeIo>(params, OpenProtocolAttributes::GetProtocol) }.ok()?;
            let mut c = Config(p);
            (c.0.segment_nr() == 0 && c.read(0, 0, 0).is_some_and(|v| v != u32::MAX)).then_some(c)
        })
    }

    /// Dword at `off` of bus 0, device `dev`, function `func`.
    pub fn read(&mut self, dev: u8, func: u8, off: u16) -> Option<u32> {
        let at = PciIoAddress::new(0, dev, func).with_extended_register(u32::from(off));
        self.0.pci().read_one::<u32>(at).ok()
    }
}
