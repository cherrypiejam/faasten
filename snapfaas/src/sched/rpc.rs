use std::net::{TcpStream, SocketAddr};
use std::thread;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use serde::{Serialize, Deserialize};

use super::Error;
use super::message;
use super::message::{Request, Response};
use super::message::request::Kind as ReqKind;

/// RPC calls
#[derive(Debug)]
pub struct Scheduler {
    _sock_addr: SocketAddr, // reconnect
    stream: TcpStream,
}

impl Scheduler {
    pub fn new(addr: String) -> Self {
        Self::try_new(addr).expect("Fail to connect to the scheduler")
    }

    pub fn try_new(addr: String) -> Result<Self, Error> {
        let stream = TcpStream::connect(&addr)
            .map_err(|e| Error::StreamConnect(e))?;
        let _sock_addr = addr.parse()
            .map_err(|e| Error::SocketAddrParse(e))?;
        Ok(Scheduler { _sock_addr, stream })
    }

    /// This method is for workers to retrieve a HTTP request, and
    /// it is supposed to block if there's no further HTTP requests
    pub fn get(&mut self) -> Result<Response, Error> {
        // avoid using unstable #![feature(thread_id_value)]
        let thread_id = {
            let mut hasher = DefaultHasher::new();
            thread::current().id().hash(&mut hasher);
            hasher.finish()
        };
        let req = Request {
            kind: Some(ReqKind::GetTask(message::GetTask { thread_id })),
        };
        message::write(&mut self.stream, &req)?;
        let response = message::read_response(&mut self.stream)?;
        Ok(response)
    }

    /// This method is for workers to return the result of a HTTP request
    pub fn finish(
        &mut self, task_id: String, result: String,
    ) -> Result<Response, Error> {
        let req = Request {
            kind: Some(ReqKind::FinishTask(message::FinishTask { task_id, result })),
        };
        message::write(&mut self.stream, &req)?;
        let response = message::read_response(&mut self.stream)?;
        Ok(response)
    }

    /// This method is for workers to invoke a function
    pub fn labeled_invoke(&mut self, labeled_invoke: message::LabeledInvoke) -> Result<(), Error> {
        let req = Request {
            kind: Some(ReqKind::LabeledInvoke(labeled_invoke))
        };
        message::write(&mut self.stream, &req)?;
        let _ = message::read_response(&mut self.stream)?;
        Ok(())
    }

    /// This method is to terminate all workers (for debug)
    pub fn terminate_all(&mut self) -> Result<(), Error> {
        let req = Request {
            kind: Some(ReqKind::TerminateAll(message::TerminateAll {})),
        };
        message::write(&mut self.stream, &req)?;
        let _ = message::read_response(&mut self.stream)?;
        Ok(())
    }

    /// This method is for local resource managers to update it's
    /// resource status, such as number of cached VMs per function
    pub fn update_resource(
        &mut self,
        info: ResourceInfo
    ) -> Result<(), Error> {
        let info = serde_json::to_vec(&info).unwrap();
        let req = Request {
            kind: Some(ReqKind::UpdateResource(message::UpdateResource { info })),
        };
        message::write(&mut self.stream, &req)?;
        let _ = message::read_response(&mut self.stream)?;
        Ok(())
    }

    /// This method is for local resrouce managers to drop itself
    pub fn drop_resource(&mut self) -> Result<(), Error> {
        let req = Request {
            kind: Some(ReqKind::DropResource(message::DropResource {})),
        };
        message::write(&mut self.stream, &req)?;
        let _ = message::read_response(&mut self.stream)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub stats: HashMap<String, usize>,
    pub total_mem: usize,
    pub free_mem: usize,
}