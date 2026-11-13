//! Printing from a session to the user's own computer.
//!
//! The session gets a printer named "Portal printer" that uses Windows' built-in
//! "Microsoft Print To PDF" driver, so a print job arrives here as a PDF. It is
//! saved in the user's transfer folder (the `\\tsclient\P` drive) and the browser
//! offers to open or print it there.
//!
//! The printer rides on the same redirection channel as the shared drive: this
//! backend passes everything about the drive to the drive backend unchanged and
//! handles only the printer.

use std::collections::HashMap;
use std::path::PathBuf;

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::efs::{
    AnyIoCtlCode, DecodedDeviceControlRequest, DeviceCloseResponse, DeviceControlRequest, DeviceCreateResponse,
    DeviceIoRequest, DeviceIoResponse, DeviceWriteResponse, Information, NtStatus, PrinterIoRequest,
    ServerDeviceAnnounceResponse, ServerDriveIoRequest,
};
use ironrdp_rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_rdpdr::pdu::esp::PortIoRequest;
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::{Rdpdr, RdpdrBackend};
use ironrdp_svc::SvcMessage;

/// Device id of the printer (the drive uses its own ids).
pub const PRINTER_DEVICE_ID: u32 = 0x5052_4e54; // "PRNT"
pub const PRINTER_NAME: &str = "Portal printer";
pub const PRINTER_DRIVER: &str = "Microsoft Print To PDF";
/// Largest print job kept.
const MAX_JOB_BYTES: usize = 200 * 1024 * 1024;

/// A finished print job, saved in the user's folder.
pub struct PrintedFile {
    pub name: String,
    pub size: usize,
}

#[derive(Debug)]
pub struct Backend {
    inner: Box<dyn RdpdrBackend>,
    dir: PathBuf,
    jobs: HashMap<u32, Option<Vec<u8>>>,
    next_file_id: u32,
    done: std::sync::mpsc::Sender<PrintedFile>,
}

impl_as_any!(Backend);

impl Backend {
    pub fn new(inner: Box<dyn RdpdrBackend>, dir: PathBuf, done: std::sync::mpsc::Sender<PrintedFile>) -> Self {
        Self { inner, dir, jobs: HashMap::new(), next_file_id: 1, done }
    }

    /// Write the job to the user's folder under a name that is not taken yet.
    fn save(&self, data: Vec<u8>) {
        let ext = if data.starts_with(b"%PDF") { "pdf" } else if data.starts_with(b"%!") { "ps" } else { "prn" };
        let stamp = chrono_like_stamp();
        let mut name = format!("Printed {stamp}.{ext}");
        let mut n = 1;
        while self.dir.join(&name).exists() {
            name = format!("Printed {stamp} ({n}).{ext}");
            n += 1;
        }
        let size = data.len();
        match std::fs::create_dir_all(&self.dir).and_then(|_| std::fs::write(self.dir.join(&name), &data)) {
            Ok(()) => {
                tracing::info!("print job saved: {name} ({size} bytes)");
                let _ = self.done.send(PrintedFile { name, size });
            }
            Err(e) => tracing::warn!("could not save a print job: {e}"),
        }
    }
}

/// "2026-11-13 18-05-42" in local time, without pulling in a date library.
fn chrono_like_stamp() -> String {
    #[cfg(windows)]
    {
        let t = crate::clock::now();
        format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
    }
    #[cfg(not(windows))]
    {
        format!("{}", crate::db::now())
    }
}

fn reply(req: DeviceIoRequest, status: NtStatus) -> DeviceIoResponse {
    DeviceIoResponse::new(req, status)
}

impl RdpdrBackend for Backend {
    fn supports_drive_security(&self) -> bool {
        self.inner.supports_drive_security()
    }
    fn reset(&mut self) -> PduResult<()> {
        self.jobs.clear();
        self.inner.reset()
    }
    fn restore_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.inner.restore_drive(device_id)
    }
    fn add_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.inner.add_drive(device_id)
    }
    fn remove_drive(&mut self, device_id: u32) -> PduResult<Vec<SvcMessage>> {
        self.inner.remove_drive(device_id)
    }
    fn handle_server_device_announce_response(&mut self, pdu: ServerDeviceAnnounceResponse) -> PduResult<()> {
        if pdu.device_id == PRINTER_DEVICE_ID {
            tracing::debug!("printer announce answered: {:?}", pdu.result_code);
            return Ok(());
        }
        self.inner.handle_server_device_announce_response(pdu)
    }
    fn handle_scard_call(&mut self, req: DeviceControlRequest<ScardIoCtlCode>, call: ScardCall) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_scard_call(req, call)
    }
    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_drive_io_request(req)
    }
    fn handle_drive_device_control(&mut self, req: DecodedDeviceControlRequest<AnyIoCtlCode>) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_drive_device_control(req)
    }
    fn poll_deferred_messages(&mut self) -> PduResult<Vec<SvcMessage>> {
        self.inner.poll_deferred_messages()
    }
    fn handle_user_logged_on(&mut self, rdpdr: &mut Rdpdr) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_user_logged_on(rdpdr)
    }
    fn handle_port_io_request(&mut self, req: PortIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.inner.handle_port_io_request(req)
    }

    fn handle_printer_io_request(&mut self, req: PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
        let pdu = match req {
            PrinterIoRequest::Create(create) => {
                let file_id = self.next_file_id;
                self.next_file_id = self.next_file_id.wrapping_add(1).max(1);
                self.jobs.insert(file_id, Some(Vec::new()));
                RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                    device_io_reply: reply(create.device_io_request, NtStatus::SUCCESS),
                    file_id,
                    information: Information::FILE_OPENED,
                })
            }
            PrinterIoRequest::Write(write) => {
                let length = write.write_data.len();
                let file_id = write.device_io_request.file_id;
                let ok = match self.jobs.get_mut(&file_id) {
                    Some(Some(job)) if job.len() + length <= MAX_JOB_BYTES => {
                        job.extend_from_slice(&write.write_data);
                        true
                    }
                    Some(slot) => {
                        *slot = None; // too large: the job is dropped
                        false
                    }
                    None => false,
                };
                RdpdrPdu::DeviceWriteResponse(DeviceWriteResponse {
                    device_io_reply: reply(write.device_io_request, if ok { NtStatus::SUCCESS } else { NtStatus::UNSUCCESSFUL }),
                    length: if ok { length as u32 } else { 0 },
                })
            }
            PrinterIoRequest::Close(close) => {
                if let Some(Some(data)) = self.jobs.remove(&close.device_io_request.file_id) {
                    if !data.is_empty() {
                        self.save(data);
                    }
                }
                RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
                    device_io_response: reply(close.device_io_request, NtStatus::SUCCESS),
                })
            }
        };
        Ok(vec![SvcMessage::from(pdu)])
    }

    fn reject_printer_write(&mut self, req: DeviceIoRequest) -> PduResult<Vec<SvcMessage>> {
        if let Some(slot) = self.jobs.get_mut(&req.file_id) {
            *slot = None;
        }
        Ok(vec![SvcMessage::from(RdpdrPdu::DeviceWriteResponse(DeviceWriteResponse {
            device_io_reply: reply(req, NtStatus::UNSUCCESSFUL),
            length: 0,
        }))])
    }
}
