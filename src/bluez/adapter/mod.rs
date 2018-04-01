mod acl_stream;
mod peripheral;

use libc;
use std;
use std::ffi::CStr;
use nom::IResult;
use bytes::{BytesMut, BufMut, LittleEndian};

use std::collections::{HashSet, HashMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use ::Result;
use api::{Characteristic, CharPropFlags, EventHandler, Event, HandleFn,
          BDAddr, AddressType, Host};

use bluez::util::handle_error;
use bluez::protocol::{hci, att};
use bluez::adapter::acl_stream::ACLStream;
use bluez::adapter::peripheral::Peripheral;
use bluez::constants::*;
use std::sync::mpsc::Sender;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::channel;
use api;


#[derive(Debug, Copy)]
#[repr(C)]
pub struct HCIDevReq {
    pub dev_id: u16,
    pub dev_opt: u32,
}

impl Clone for HCIDevReq {
    fn clone(&self) -> Self { *self }
}

#[derive(Copy)]
#[repr(C)]
pub struct HCIDevStats {
    pub err_rx : u32,
    pub err_tx : u32,
    pub cmd_tx : u32,
    pub evt_rx : u32,
    pub acl_tx : u32,
    pub acl_rx : u32,
    pub sco_tx : u32,
    pub sco_rx : u32,
    pub byte_rx : u32,
    pub byte_tx : u32,
}

impl Clone for HCIDevStats{
    fn clone(&self) -> Self { *self }
}

impl HCIDevStats {
    fn default() -> HCIDevStats {
        HCIDevStats {
            err_rx: 0u32,
            err_tx: 0u32,
            cmd_tx: 0u32,
            evt_rx: 0u32,
            acl_tx: 0u32,
            acl_rx: 0u32,
            sco_tx: 0u32,
            sco_rx: 0u32,
            byte_rx: 0u32,
            byte_tx: 0u32
        }
    }
}

#[derive(Copy)]
#[repr(C)]
pub struct HCIDevInfo {
    pub dev_id : u16,
    pub name : [libc::c_char; 8],
    pub bdaddr : BDAddr,
    pub flags : u32,
    pub type_ : u8,
    pub features : [u8; 8],
    pub pkt_type : u32,
    pub link_policy : u32,
    pub link_mode : u32,
    pub acl_mtu : u16,
    pub acl_pkts : u16,
    pub sco_mtu : u16,
    pub sco_pkts : u16,
    pub stat : HCIDevStats,
}

impl Clone for HCIDevInfo {
    fn clone(&self) -> Self { *self }
}

impl HCIDevInfo {
    pub fn default() -> HCIDevInfo {
        HCIDevInfo {
            dev_id: 0,
            name: [0i8; 8],
            bdaddr: BDAddr { address: [0u8; 6] },
            flags: 0u32,
            type_: 0u8,
            features: [0u8; 8],
            pkt_type: 0u32,
            link_policy: 0u32,
            link_mode: 0u32,
            acl_mtu: 0u16,
            acl_pkts: 0u16,
            sco_mtu: 0u16,
            sco_pkts: 0u16,
            stat: HCIDevStats::default()
        }
    }
}

#[derive(Copy, Debug)]
#[repr(C)]
pub struct SockaddrHCI {
    hci_family: libc::sa_family_t,
    hci_dev: u16,
    hci_channel: u16,
}

impl Clone for SockaddrHCI {
    fn clone(&self) -> Self { *self }
}


#[derive(Debug, Copy, Clone)]
pub enum AdapterType {
    BrEdr,
    Amp,
    Unknown(u8)
}

impl AdapterType {
    fn parse(typ: u8) -> AdapterType {
        match typ {
            0 => AdapterType::BrEdr,
            1 => AdapterType::Amp,
            x => AdapterType::Unknown(x),
        }
    }

    fn num(&self) -> u8 {
        match *self {
            AdapterType::BrEdr => 0,
            AdapterType::Amp => 1,
            AdapterType::Unknown(x) => x,
        }
    }
}

#[derive(Hash, Eq, PartialEq, Debug, Copy, Clone)]
pub enum AdapterState {
    Up, Init, Running, Raw, PScan, IScan, Inquiry, Auth, Encrypt
}

impl AdapterState {
    fn parse(flags: u32) -> HashSet<AdapterState> {
        use self::AdapterState::*;

        let states = [Up, Init, Running, Raw, PScan, IScan, Inquiry, Auth, Encrypt];

        let mut set = HashSet::new();
        for (i, f) in states.iter().enumerate() {
            if flags & (1 << (i & 31)) != 0 {
                set.insert(f.clone());
            }
        }

        set
    }
}

#[derive(Debug, Default)]
struct DeviceState {
    address: BDAddr,
    address_type: AddressType,
    local_name: Option<String>,
    tx_power_level: Option<i8>,
    manufacturer_data: Option<Vec<u8>>,
    discovery_count: u32,
    has_scan_response: bool,
    characteristics: BTreeSet<Characteristic>,
}

#[derive(Clone)]
pub struct ConnectedAdapter {
    pub adapter: Adapter,
    adapter_fd: i32,
    should_stop: Arc<AtomicBool>,
    pub scan_enabled: Arc<AtomicBool>,
    event_txs: Arc<Mutex<Vec<Sender<Event>>>>,
    peripherals: Arc<Mutex<HashMap<BDAddr, Peripheral>>>,
}

impl ConnectedAdapter {
    pub fn new(adapter: &Adapter) -> Result<ConnectedAdapter> {
        let adapter_fd = handle_error(unsafe {
            libc::socket(libc::AF_BLUETOOTH, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 1)
        })?;

        let addr = SockaddrHCI {
            hci_family: libc::AF_BLUETOOTH as u16,
            hci_dev: adapter.dev_id,
            hci_channel: 0,
        };

        handle_error(unsafe {
            libc::bind(adapter_fd, &addr as *const SockaddrHCI as *const libc::sockaddr,
                       std::mem::size_of::<SockaddrHCI>() as u32)
        })?;

        let should_stop = Arc::new(AtomicBool::new(false));

        let connected = ConnectedAdapter {
            adapter: adapter.clone(),
            adapter_fd,
            should_stop,
            scan_enabled: Arc::new(AtomicBool::new(false)),
            event_txs: Arc::new(Mutex::new(vec![])),
            peripherals: Arc::new(Mutex::new(HashMap::new())),
        };

        connected.add_raw_socket_reader(adapter_fd);

        connected.set_socket_filter()?;

        Ok(connected)
    }

    fn set_socket_filter(&self) -> Result<()> {
        let mut filter = BytesMut::with_capacity(14);
        let type_mask = (1 << HCI_COMMAND_PKT) | (1 << HCI_EVENT_PKT) | (1 << HCI_ACLDATA_PKT);
        let event_mask1 = (1 << EVT_DISCONN_COMPLETE) | (1 << EVT_ENCRYPT_CHANGE) |
            (1 << EVT_CMD_COMPLETE) | (1 << EVT_CMD_STATUS);
        let event_mask2 = 1 << (EVT_LE_META_EVENT - 32);
        let opcode = 0;

        filter.put_u32::<LittleEndian>(type_mask);
        filter.put_u32::<LittleEndian>(event_mask1);
        filter.put_u32::<LittleEndian>(event_mask2);
        filter.put_u32::<LittleEndian>(opcode);

        handle_error(unsafe {
            libc::setsockopt(self.adapter_fd, SOL_HCI, HCI_FILTER,
                             filter.as_mut_ptr() as *mut _ as *mut libc::c_void,
                             filter.len() as u32)
        })?;
        Ok(())
    }

    fn add_raw_socket_reader(&self, fd: i32) {
        let should_stop = self.should_stop.clone();
        let connected = self.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut cur: Vec<u8> = vec![];

            while !should_stop.load(Ordering::Relaxed) {
                // debug!("reading");
                let len = handle_error(unsafe {
                    libc::read(fd, buf.as_mut_ptr() as *mut _ as *mut libc::c_void, buf.len()) as i32
                }).unwrap_or(0) as usize;
                if len == 0 {
                    continue;
                }

                cur.put_slice(&buf[0..len]);

                let mut new_cur: Option<Vec<u8>> = Some(vec![]);
                {
                    let result = {
                        hci::message(&cur)
                    };

                    match result {
                        IResult::Done(left, result) => {
                            ConnectedAdapter::handle(&connected, result);
                            if !left.is_empty() {
                                new_cur = Some(left.to_owned());
                            };
                        }
                        IResult::Incomplete(_) => {
                            new_cur = None;
                        },
                        IResult::Error(err) => {
                            error!("parse error {}\nfrom: {:?}", err, cur);
                        }
                    }
                };

                cur = new_cur.unwrap_or(cur);
            }
        });
    }

    fn emit(&self, event: Event) {
        let mut txs = self.event_txs.lock().unwrap();
        txs.retain(|tx|{
            // drop the tx if we see an error; that means the other end has hung up
            tx.send(event.clone()).is_ok()
        });
    }

    fn handle(&self, message: hci::Message) {
        debug!("got message {:?}", message);

        let mut peripherals = self.peripherals.lock().unwrap();

        match message {
            hci::Message::LEAdvertisingReport(info) => {
                use bluez::protocol::hci::LEAdvertisingData::*;

                let mut new = false;

                let peripheral = peripherals.entry(info.bdaddr)
                    .or_insert_with(|| {
                        new = true;
                        Peripheral::new(self.clone(), info.bdaddr)
                    });

                let address = info.bdaddr.clone();

                peripheral.handle_device_message(&hci::Message::LEAdvertisingReport(info));

                if new {
                    self.emit(Event::DeviceDiscovered(address.clone()))
                } else {
                    self.emit(Event::DeviceUpdated(address.clone()))
                }
            }
            hci::Message::LEConnComplete(info) => {
                info!("connected to {:?}", info);
                let address = info.bdaddr.clone();

                match peripherals.get(&info.bdaddr) {
                    Some(peripheral) => {
                        peripheral.handle_device_message(&hci::Message::LEConnComplete(info))
                    }
                    // todo: there's probably a better way to handle this case
                    None => warn!("Got connection for unknown device {}", info.bdaddr)
                }

                self.emit(Event::DeviceConnected(address));
            }
            hci::Message::ACLDataPacket(data) => {
                let message = hci::Message::ACLDataPacket(data);
                for peripheral in peripherals.values() {
                    // we don't know the handler => device mapping, so send to all and let them filter
                    peripheral.handle_device_message(&message);
                }
            },
            hci::Message::DisconnectComplete { handle, .. } => {
                // TODO: handle disconnects
            }
            _ => {
                // skip
            }
        }
    }

    fn write(&self, message: &mut [u8]) -> Result<()> {
        debug!("writing({}) {:?}", self.adapter_fd, message);
        let ptr = message.as_mut_ptr();
        handle_error(unsafe {
            libc::write(self.adapter_fd, ptr as *mut _ as *mut libc::c_void, message.len()) as i32
        })?;
        Ok(())
    }

    fn set_scan_params(&self) -> Result<()> {
        let mut data = BytesMut::with_capacity(7);
        data.put_u8(1); // scan_type = active
        data.put_u16::<LittleEndian>(0x0010); // interval ms
        data.put_u16::<LittleEndian>(0x0010); // window ms
        data.put_u8(0); // own_type = public
        data.put_u8(0); // filter_policy = public
        let mut buf = hci::hci_command(LE_SET_SCAN_PARAMETERS_CMD, &*data);
        self.write(&mut *buf)
    }

    fn set_scan_enabled(&self, enabled: bool) -> Result<()> {
        let mut data = BytesMut::with_capacity(2);
        data.put_u8(if enabled { 1 } else { 0 }); // enabled
        data.put_u8(1); // filter duplicates

        self.scan_enabled.clone().store(enabled, Ordering::Relaxed);
        let mut buf = hci::hci_command(LE_SET_SCAN_ENABLE_CMD, &*data);
        self.write(&mut *buf)
    }

//    fn discover_chars_in_range_int(&self, address: BDAddr, start: u16, end: u16) {
//        let mut buf = BytesMut::with_capacity(7);
//        buf.put_u8(ATT_OP_READ_BY_TYPE_REQ);
//        buf.put_u16::<LittleEndian>(start);
//        buf.put_u16::<LittleEndian>(end);
//        buf.put_u16::<LittleEndian>(GATT_CHARAC_UUID);
//
//        let self_copy = self.clone();
//        let handler = Box::new(move |_: u16, data: &[u8]| {
//            match att::characteristics(data).to_result() {
//                Ok(chars) => {
//                    debug!("Chars: {:#?}", chars);
//                    let mut devices = self_copy.discovered.lock().unwrap();
//                    devices.get_mut(&address).as_mut().map(|ref mut d| {
//                        let mut next = None;
//                        let mut char_set = d.characteristics.clone();
//                        chars.into_iter().for_each(|mut c| {
//                            c.end_handle = end;
//                            next = Some(c.start_handle);
//                            char_set.insert(c);
//                        });
//
//                        // fix the end handles
//                        let mut prev = 0xffff;
//                        d.characteristics = char_set.into_iter().rev().map(|mut c| {
//                            c.end_handle = prev;
//                            prev = c.start_handle - 1;
//                            c
//                        }).collect();
//
//                        next.map(|next| {
//                            if next < end {
//                                self_copy.discover_chars_in_range_int(address, next + 1, end);
//                            }
//                        });
//                    }).or_else(|| {
//                        warn!("received chars for unknown device: {}", address);
//                        None
//                    });
//                }
//                Err(err) => {
//                    error!("failed to parse chars: {:?}", err);
//                }
//            };
//        });
//
//        self.write_acl_packet(address, &mut *buf, Some(handler));
//    }
//
//    fn request_by_handle(&self, address: BDAddr, handle: u16, data: &[u8],
//                         handler: Option<HandleFn>) {
//        let streams = self.streams.lock().unwrap();
//        let stream = streams.get(&address).unwrap();
//        let mut buf = BytesMut::with_capacity(3 + data.len());
//        buf.put_u8(ATT_OP_WRITE_REQ);
//        buf.put_u16::<LittleEndian>(handle);
//        buf.put(data);
//        stream.write(&mut *buf, handler);
//    }
//
//    fn notify(&self, address: BDAddr, char: &Characteristic, enable: bool) {
//        info!("setting notify for {}/{:?} to {}", address, char.uuid, enable);
//        let streams = self.streams.lock().unwrap();
//        let stream = streams.get(&address).unwrap();
//
//        let mut buf = BytesMut::with_capacity(7);
//        buf.put_u8(ATT_OP_READ_BY_TYPE_REQ);
//        buf.put_u16::<LittleEndian>(char.start_handle);
//        buf.put_u16::<LittleEndian>(char.end_handle);
//        buf.put_u16::<LittleEndian>(GATT_CLIENT_CHARAC_CFG_UUID);
//        let self_copy = self.clone();
//        let char_copy = char.clone();
//
//        stream.write(&mut *buf, Some(Box::new(move |_, data| {
//            match att::notify_response(data).to_result() {
//                Ok(resp) => {
//                    debug!("got notify response: {:?}", resp);
//
//                    let use_notify = char_copy.properties.contains(CharPropFlags::NOTIFY);
//                    let use_indicate = char_copy.properties.contains(CharPropFlags::INDICATE);
//
//                    let mut value = resp.value;
//
//                    if enable {
//                        if use_notify {
//                            value |= 0x0001;
//                        } else if use_indicate {
//                            value |= 0x0002;
//                        }
//                    } else {
//                        if use_notify {
//                            value &= 0xFFFE;
//                        } else if use_indicate {
//                            value &= 0xFFFD;
//                        }
//                    }
//
//                    let mut value_buf = BytesMut::with_capacity(2);
//                    value_buf.put_u16::<LittleEndian>(value);
//                    self_copy.request_by_handle(address, resp.handle,
//                                                &*value_buf, Some(Box::new(|_, data| {
//                            if data.len() > 0 && data[0] == ATT_OP_WRITE_RESP {
//                                debug!("Got response from notify: {:?}", data);
//                            } else {
//                                warn!("Unexpected notify response: {:?}", data);
//                            }
//                        })));
//                }
//                Err(err) => {
//                    error!("failed to parse notify response: {:?}", err);
//                }
//            };
//        })));
//    }
//
//
//    pub fn disconnect(&self, address: BDAddr) -> Result<()> {
//        let handle = {
//            let stream = self.streams.lock().unwrap();
//            stream.get(&address).unwrap().handle
//        };
//
//        let mut data = BytesMut::with_capacity(3);
//        data.put_u16::<LittleEndian>(handle);
//        data.put_u8(HCI_OE_USER_ENDED_CONNECTION);
//        let mut buf = hci::hci_command(DISCONNECT_CMD, &*data);
//        self.write(&mut *buf)
//    }
//
//    pub fn command(&self, address: BDAddr, char: &Characteristic, data: &[u8]) {
//        let streams = self.streams.lock().unwrap();
//        let stream = streams.get(&address).unwrap();
//        let mut buf = BytesMut::with_capacity(3 + data.len());
//        buf.put_u8(ATT_OP_WRITE_CMD);
//        buf.put_u16::<LittleEndian>(char.value_handle);
//        buf.put(data);
//        stream.write_cmd(&mut *buf);
//    }
//
////    fn device(&self, address: BDAddr) -> Option<Device> {
////        let discovered = self.discovered.lock().unwrap();
////        discovered.get(&address).map(|d| d.to_device())
////    }
//
//    pub fn discover_chars(&self, address: BDAddr) {
//        self.discover_chars_in_range(address, 0x0001, 0xFFFF);
//    }
//
//    pub fn discover_chars_in_range(&self, address: BDAddr, start: u16, end: u16) {
//        self.discover_chars_in_range_int(address, start, end);
//    }
//
//    pub fn request(&self, address: BDAddr, char: &Characteristic, data: &[u8],
//               handler: Option<HandleFn>) {
//        self.request_by_handle(address, char.value_handle, data, handler);
//    }
//
//    pub fn subscribe(&self, address: BDAddr, char: &Characteristic) {
//        self.notify(address, char, true);
//    }
//
//    pub fn unsubscribe(&self, address: BDAddr, char: &Characteristic) {
//        self.notify(address, char, false);
//    }
}

impl Host for ConnectedAdapter {
    fn event_stream(&self) -> Receiver<Event> {
        let mut list = self.event_txs.lock().unwrap();
        let (tx, rx) = channel();
        (*list).push(tx);
        rx
    }

    fn start_scan(&self) -> Result<()> {
        self.set_scan_params()?;
        self.set_scan_enabled(true)
    }

    fn stop_scan(&self) -> Result<()> {
        self.set_scan_enabled(false)
    }

    fn peripherals(&self) -> Vec<Box<api::Peripheral>> {
        unimplemented!();
    }

    fn peripheral(&self, _address: BDAddr) -> Option<Box<api::Peripheral>> {
        unimplemented!();
    }

//    fn discovered(&self) -> Vec<Device> {
//        let discovered = self.discovered.lock().unwrap();
//        discovered.values().map(|d| d.to_device()).collect()
//    }

}

#[derive(Debug, Clone)]
pub struct Adapter {
    pub name: String,
    pub dev_id: u16,
    pub addr: BDAddr,
    pub typ: AdapterType,
    pub states: HashSet<AdapterState>,
}

// #define HCIGETDEVINFO	_IOR('H', 211, int)
static HCI_GET_DEV_MAGIC: usize = (2u32 << 0i32 + 8i32 + 8i32 + 14i32 |
    (b'H' as (i32) << 0i32 + 8i32) as (u32) | (211i32 << 0i32) as (u32)) as (usize) |
    4 /* (sizeof(i32)) */ << 0i32 + 8i32 + 8i32;

impl Adapter {
    pub fn from_device_info(di: &HCIDevInfo) -> Adapter {
        Adapter {
            name: String::from(unsafe { CStr::from_ptr(di.name.as_ptr()).to_str().unwrap() }),
            dev_id: 0,
            addr: di.bdaddr,
            typ: AdapterType::parse((di.type_ & 0x30) >> 4),
            states: AdapterState::parse(di.flags),
        }
    }

    pub fn from_dev_id(ctl: i32, dev_id: u16) -> Result<Adapter> {
        let mut di = HCIDevInfo::default();
        di.dev_id = dev_id;

        unsafe {
            handle_error(libc::ioctl(ctl, HCI_GET_DEV_MAGIC as libc::c_ulong,
                                     &mut di as (*mut HCIDevInfo) as (*mut libc::c_void)))?;
        }

        Ok(Adapter::from_device_info(&di))
    }

    pub fn is_up(&self) -> bool {
        self.states.contains(&AdapterState::Up)
    }

    pub fn connect(&self) -> Result<ConnectedAdapter> {
        ConnectedAdapter::new(self)
    }
}
