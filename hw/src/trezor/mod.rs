#![allow(dead_code)]
#![allow(unused_imports)]

// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Trezor hardware wallet module. Supports Trezor v1.
//! See http://doc.satoshilabs.com/trezor-tech/api-protobuf.html
//! and https://github.com/trezor/trezor-common/blob/master/protob/protocol.md
//! for protocol details.

use super::WalletInfo;
use bigint::hash::{H256, H160};
use ethkey::{Address, Signature};
use rlp;

use hidapi;
use protobuf;
use protobuf::{Message, MessageStatic, ProtobufEnum};
use std::cmp::min;
use std::fmt;
use std::sync::Arc;
use parking_lot::Mutex;
use std::str::FromStr;
use std::time::Duration;
use serde_json;
use super::TransactionInfo;

mod gen;
use self::gen::messages::*;

const TREZOR_VID: u16 = 0x534c;
const TREZOR_PIDS: [u16; 1] = [0x0001]; // Trezor v1, keeping this as an array to leave room for Trezor v2 which is in progress
const ETH_DERIVATION_PATH: [u32; 4] = [0x8000002C, 0x8000003C, 0x80000000, 0]; // m/44'/60'/0'/0
const ETC_DERIVATION_PATH: [u32; 4] = [0x8000002C, 0x8000003D, 0x80000000, 0]; // m/44'/61'/0'/0

#[cfg(windows)]
const HID_PREFIX_ZERO: bool = true;
#[cfg(not(windows))]
const HID_PREFIX_ZERO: bool = false;

/// Key derivation paths used on ledger wallets.
#[derive(Debug, Clone, Copy)]
pub enum KeyPath {
	/// Ethereum.
	Ethereum,
	/// Ethereum classic.
	EthereumClassic,
}

/// Hardware wallet error.
#[derive(Debug)]
pub enum Error {
	/// Ethereum wallet protocol error.
	Protocol(&'static str),
	/// Hidapi error.
	Usb(hidapi::HidError),
	/// Device with request key is not available.
	KeyNotFound,
	/// Signing has been cancelled by user.
	UserCancel,
	BadMessageType,
	SerdeError(serde_json::Error),
	ClosedDevice(String),
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		match *self {
			Error::Protocol(ref s) => write!(f, "Trezor protocol error: {}", s),
			Error::Usb(ref e) => write!(f, "USB communication error: {}", e),
			Error::KeyNotFound => write!(f, "Key not found"),
			Error::UserCancel => write!(f, "Operation has been cancelled"),
			Error::BadMessageType => write!(f, "Bad Message Type in RPC call"),
			Error::SerdeError(ref e) => write!(f, "Serde serialization error: {}", e),
			Error::ClosedDevice(ref s) => write!(f, "Device is closed, needs PIN to perform operations: {}", s),
		}
	}
}

impl From<hidapi::HidError> for Error {
	fn from(err: hidapi::HidError) -> Error {
		Error::Usb(err)
	}
}

impl From<protobuf::ProtobufError> for Error {
	fn from(_: protobuf::ProtobufError) -> Error {
		Error::Protocol(&"Could not read response from Trezor Device")
	}
}

/// Ledger device manager.
pub struct Manager {
	usb: Arc<Mutex<hidapi::HidApi>>,
	devices: Vec<Device>,
	closed_devices: Vec<String>,
	key_path: KeyPath,
}

#[derive(Debug)]
struct Device {
	path: String,
	info: WalletInfo,
}

impl Manager {
	/// Create a new instance.
	pub fn new(hidapi: Arc<Mutex<hidapi::HidApi>>) -> Manager {
		Manager {
			usb: hidapi,
			devices: Vec::new(),
			closed_devices: Vec::new(),
			key_path: KeyPath::Ethereum,
		}
	}

	/// Re-populate device list
	pub fn update_devices(&mut self) -> Result<usize, Error> {
		let mut usb = self.usb.lock();
		usb.refresh_devices();
		let devices = usb.devices();
		let mut new_devices = Vec::new();
		let mut closed_devices = Vec::new();
		for usb_device in devices {
			trace!("Checking device: {:?}", usb_device);
			if usb_device.vendor_id != TREZOR_VID || !TREZOR_PIDS.contains(&usb_device.product_id) || usb_device.usage_page != 0xFF00 {
				continue;
			}
			match self.read_device_info(&usb, &usb_device) {
				Ok(device) => new_devices.push(device),
				Err(Error::ClosedDevice(path)) => closed_devices.push(path.to_string()),
				Err(e) => return Err(e),
			}
		}
		let count = new_devices.len();
		self.devices = new_devices;
		self.closed_devices = closed_devices;
		Ok(count)
	}

	fn read_device_info(&self, usb: &hidapi::HidApi, dev_info: &hidapi::HidDeviceInfo) -> Result<Device, Error> {
		let handle = self.open_path(|| usb.open_path(&dev_info.path))?;
		let manufacturer = dev_info.manufacturer_string.clone().unwrap_or("Unknown".to_owned());
		let name = dev_info.product_string.clone().unwrap_or("Unknown".to_owned());
		let serial = dev_info.serial_number.clone().unwrap_or("Unknown".to_owned());
		match self.get_address(&handle) {
			Ok(Some(addr)) => {
				Ok(Device {
					path: dev_info.path.clone(),
					info: WalletInfo {
						name: name,
						manufacturer: manufacturer,
						serial: serial,
						address: addr,
					},
				})
			}
			Ok(None) => Err(Error::ClosedDevice(dev_info.path.clone())),
			Err(e) => Err(e)
		}
	}

	pub fn message(&self, message_type: String, device_path: Option<String>, message: Option<String>) -> Result<String, Error> {
		match message_type.as_ref() {
			"get_devices" => {
				serde_json::to_string(&self.closed_devices).map_err(Error::SerdeError)
			}
			"pin_matrix_ack" => {
				if let (Some(path), Some(msg)) = (device_path, message) {
					let unlocked = self.pin_matrix_ack(&path, &msg)?;
					serde_json::to_string(&unlocked).map_err(Error::SerdeError)
				} else {
					Err(Error::BadMessageType)
				}
			}
			_ => Err(Error::BadMessageType)
		}
	}

	/// Select key derivation path for a known chain.
	pub fn set_key_path(&mut self, key_path: KeyPath) {
		self.key_path = key_path;
	}

	/// List connected wallets. This only returns wallets that are ready to be used.
	pub fn list_devices(&self) -> Vec<WalletInfo> {
		self.devices.iter().map(|d| d.info.clone()).collect()
	}

	/// Get wallet info.
	pub fn device_info(&self, address: &Address) -> Option<WalletInfo> {
		self.devices.iter().find(|d| &d.info.address == address).map(|d| d.info.clone())
	}

	fn open_path<R, F>(&self, f: F) -> Result<R, Error>
	where F: Fn() -> Result<R, &'static str> {
		let mut err = Error::KeyNotFound;
		/// Try to open device a few times.
		for _ in 0..10 {
			match f() {
				Ok(handle) => return Ok(handle),
				Err(e) => err = From::from(e),
			}
			::std::thread::sleep(Duration::from_millis(200));
		}
		Err(err)
	}

	fn pin_matrix_ack(&self, device_path: &str, pin: &str) -> Result<bool, Error> {
		let usb = self.usb.lock();
		let device = self.open_path(|| usb.open_path(&device_path))?;
		let t = MessageType::MessageType_PinMatrixAck;
		let mut m = PinMatrixAck::new();
		m.set_pin(pin.to_string());
		self.send_device_message(&device, &t, &m)?;
		let (resp_type, bytes) = self.read_device_response(&device)?;
		match resp_type {
			// Getting an Address back means it's unlocked, this is undocumented behavior
			MessageType::MessageType_EthereumAddress => {
				Ok(true)
			}
			// Getting anything else means we didn't unlock it
			_ => {
				Ok(false)
			}
		}
	}

	fn get_address(&self, device: &hidapi::HidDevice) -> Result<Option<Address>, Error> {
		let typ = MessageType::MessageType_EthereumGetAddress;
		let mut message = EthereumGetAddress::new();
		match self.key_path {
			KeyPath::Ethereum => message.set_address_n(ETH_DERIVATION_PATH.to_vec()),
			KeyPath::EthereumClassic => message.set_address_n(ETC_DERIVATION_PATH.to_vec()),
		}
		message.set_show_display(false);
		self.send_device_message(&device, &typ, &message)?;

		let (resp_type, bytes) = self.read_device_response(&device)?;
		match resp_type {
			MessageType::MessageType_EthereumAddress => {
				let response: EthereumAddress = protobuf::core::parse_from_bytes(&bytes)?;
				Ok(Some(From::from(response.get_address())))
			}
			_ => Ok(None)
		}
	}

	/// Sign transaction data with wallet managing `address`.
	pub fn sign_transaction(&self, address: &Address, t_info: &TransactionInfo) -> Result<Signature, Error> {
		let device = self.devices.iter().find(|d| &d.info.address == address)
			.ok_or(Error::KeyNotFound)?;
		println!("T info: {:?}", t_info);
		let usb = self.usb.lock();
		let mut handle = self.open_path(|| usb.open_path(&device.path))?;
		let msg_type = MessageType::MessageType_EthereumSignTx;
		let mut message = EthereumSignTx::new();
		match self.key_path {
			KeyPath::Ethereum => message.set_address_n(ETH_DERIVATION_PATH.to_vec()),
			KeyPath::EthereumClassic => message.set_address_n(ETC_DERIVATION_PATH.to_vec()),
		}
		// This encoding is completely undocumented, documentation says it
		// should just be a big-endian unsigned integer, but it's actually an
		// RLP encoded integer _without_ the initial length byte. This was found
		// by trial-and-error and inspecting their sample python code.
		message.set_nonce(rlp::encode(&t_info.nonce)[1..].to_vec());
		message.set_gas_limit(rlp::encode(&t_info.gas_limit)[1..].to_vec());
		message.set_gas_price(rlp::encode(&t_info.gas_price)[1..].to_vec());
		message.set_value(rlp::encode(&t_info.value)[1..].to_vec());

		match t_info.to {
			Some(addr) => {
				message.set_to(addr.to_vec())
			},
			None => ()
		}
		let first_chunk_length = min(t_info.data.len(), 1024);
		let chunk = &t_info.data[0..first_chunk_length];
		println!("Chunk: {:?}", chunk);
		message.set_data_initial_chunk(chunk.to_vec());
		message.set_data_length(t_info.data.len() as u32);
		if let Some(n_id) = t_info.network_id {
			message.set_chain_id(n_id as u32);
		}

		self.send_device_message(&handle, &msg_type, &message)?;

		let sig = self.signing_loop(&handle, &t_info.network_id, &t_info.data[first_chunk_length..])?;
		Ok(sig)
	}

	fn signing_loop(&self, handle: &hidapi::HidDevice, chain_id: &Option<u64>, data: &[u8]) -> Result<Signature, Error> {
		let (resp_type, bytes) = self.read_device_response(&handle)?;
		match resp_type {
			MessageType::MessageType_Cancel => Err(Error::UserCancel),
			MessageType::MessageType_ButtonRequest => {
				self.send_device_message(handle, &MessageType::MessageType_ButtonAck, &ButtonAck::new())?;
				::thread::sleep(Duration::from_millis(200));
				self.signing_loop(handle, chain_id, data)
			}
			MessageType::MessageType_EthereumTxRequest => {
				let resp: EthereumTxRequest = protobuf::core::parse_from_bytes(&bytes)?;
				if resp.has_data_length() {
					let mut msg = EthereumTxAck::new();
					let len = resp.get_data_length() as usize;
					msg.set_data_chunk(data[..len].to_vec());
					self.send_device_message(handle, &MessageType::MessageType_EthereumTxAck, &msg)?;
					self.signing_loop(handle, chain_id, &data[len..])
				} else {
					let v = resp.get_signature_v();
					let r = H256::from_slice(resp.get_signature_r());
					let s = H256::from_slice(resp.get_signature_s());
					if let Some(c_id) = *chain_id {
						// If there is a chain_id supplied, Trezor will return a v
						// part of the signature that is already adjusted for EIP-155,
						// so v' = v + 2 * chain_id + 35, but code further down the
						// pipeline will already do this transformation, so remove it here
						Ok(Signature::from_rsv(&r, &s, (v - (35 + 2 * c_id as u32)) as u8))
					} else {
						// If there isn't a chain_id, v will be returned as v + 27
						Ok(Signature::from_rsv(&r, &s, (v - 27) as u8))
					}
				}
			}
			MessageType::MessageType_Failure => {
				let mut resp: Failure = protobuf::core::parse_from_bytes(&bytes)?;
				Err(Error::Protocol("Last message sent failed"))
			}
			_ => Err(Error::Protocol("Unexpected response from Trezor device."))
		}
	}

	fn send_device_message(&self, device: &hidapi::HidDevice, msg_type: &MessageType, msg: &Message) -> Result<usize, Error> {
		let msg_id = *msg_type as u16;
		let mut message = msg.write_to_bytes()?;
		let msg_size = message.len();
		let mut data = Vec::new();
		// Magic constants
		data.push('#' as u8);
		data.push('#' as u8);
		data.push(((msg_id >> 8) & 0xFF) as u8); // First byte of BE msg type
		data.push((msg_id & 0xFF) as u8); // Second byte of BE msg type
		// Convert msg_size to BE and split into bytes
		data.push(((msg_size >> 24) & 0xFF) as u8);
		data.push(((msg_size >> 16) & 0xFF) as u8);
		data.push(((msg_size >> 8) & 0xFF) as u8);
		data.push((msg_size & 0xFF) as u8);
		data.append(&mut message);
		while data.len() % 63 > 0 {
			data.push(0);
		}
		let mut total_written = 0;
		for chunk in data.chunks(63) {
			let mut padded_chunk = vec![0, '?' as u8]; // TODO: Determine HID_Version and pad or not depending on that
			padded_chunk.extend_from_slice(&chunk);
			total_written += device.write(&padded_chunk)?;
		}
		Ok(total_written)
	}

	fn read_device_response(&self, device: &hidapi::HidDevice) -> Result<(MessageType, Vec<u8>), Error> {
		let protocol_err = Error::Protocol(&"Unexpected wire response from Trezor Device");
		let mut buf = vec![0; 64];

		let first_chunk = device.read_timeout(&mut buf, 10_000)?;
		if first_chunk < 9 || buf[0] != '?' as u8 || buf[1] != '#' as u8 || buf[2] != '#' as u8 {
			return Err(protocol_err);
		}
		let msg_type = MessageType::from_i32(((buf[3] as i32 & 0xFF) << 8) + (buf[4] as i32 & 0xFF)).ok_or(protocol_err)?;
		let msg_size = ((buf[5] as u32 & 0xFF) << 24) + ((buf[6] as u32 & 0xFF) << 16) + ((buf[7] as u32 & 0xFF) << 8) + (buf[8] as u32 & 0xFF);
		let mut data = Vec::new();
		data.extend_from_slice(&buf[9..]);
		while data.len() < (msg_size as usize) {
			device.read_timeout(&mut buf, 10_000)?;
			data.extend_from_slice(&buf[1..]);
		}
		Ok((msg_type, data[..msg_size as usize].to_vec()))
	}
}

#[test]
fn debug() {
	use util::{U256};

	let hidapi = Arc::new(Mutex::new(hidapi::HidApi::new().unwrap()));
	let mut manager = Manager::new(hidapi.clone());
	let addr: Address = H160::from("3C9b5aC40587E6799D42f7342c3641bc4aABEDa4");

	manager.update_devices().unwrap();

	let t_info = TransactionInfo {
		nonce: U256::zero(),
		gas_price: U256::from(100),
		gas_limit: U256::from(21_000),
		to: Some(H160::from("00b1d5c8e02a18f5d5ddb83b6d17db757706148c")),
		value: U256::from(1_000_000),
		data: (&[1u8;3000]).to_vec(),
	};
	let signature = manager.sign_transaction(&addr, &t_info);
	println!("Signature: {:?}", signature);

	assert!(true)
}
