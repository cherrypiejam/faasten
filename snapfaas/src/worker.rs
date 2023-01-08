//! Workers proxies requests and responses between the request manager and VMs.
//! Each worker runs in its own thread and is modeled as the following state
//! machine:
use std::sync::mpsc::Sender;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use log::{error, debug};
use time::precise_time_ns;

use crate::message::Message;
use crate::request::{RequestStatus, LabeledInvoke, Response};
use crate::vm;
use crate::metrics::{self, RequestTimestamps};
use crate::resource_manager;
use crate::fs;
use crate::sched;
use crate::sched::rpc::Scheduler;

// one hour
const FLUSH_INTERVAL_SECS: u64 = 3600;


#[derive(Debug)]
pub struct Worker {
    pub thread: JoinHandle<()>,
}

fn handle_request(
    req: LabeledInvoke,
    sched_rpc: Arc<Mutex<Scheduler>>,
    vm_req_sender: Sender<Message>,
    vm_listener: UnixListener,
    mut tsps: RequestTimestamps,
    stat: &mut metrics::WorkerMetrics,
    cid: u32,
) -> Response {
    debug!("invoke: {:?}", &req);

    tsps.arrived = precise_time_ns();

    fs::utils::clear_label();
    fs::utils::taint_with_label(labeled::buckle::Buckle::new(req.label.secrecy, true));
    fs::utils::set_my_privilge(req.gate.privilege);
    let function_name = req.gate.image;
    let mut i = 0;
    let result = loop {
        let mut tsps = tsps.clone();
        if i == 5 {
            break RequestStatus::ProcessRequestFailed;
        }
        i += 1;
        let (tx, rx) = mpsc::channel();
        vm_req_sender.send(Message::GetVm(function_name.clone(), tx)).expect("Failed to send GetVm request");
        match rx.recv().expect("Failed to receive GetVm response") {
            Ok(mut vm) => {
                // TODO: label cached VM
                tsps.allocated = precise_time_ns();
                if !vm.is_launched() {
                    // newly allocated VM is returned, launch it first
                    if let Err(e) = vm.launch(
                        Some(Arc::clone(&sched_rpc)),
                        vm_listener.try_clone().expect("clone unix listener"),
                        cid, false,
                        None,
                    ) {
                        handle_vm_error(e);
                        // TODO send response back to gateway
                        // let _ = rsp_sender.send(Response {
                            // status: RequestStatus::LaunchFailed,
                        // });

                        // a VM launched or not occupies system resources, we need
                        // to put back the resources assigned to this VM.
                        vm_req_sender.send(Message::DeleteVm(vm)).expect("Failed to send DeleteVm request");
                        // insert the request's timestamps
                        stat.push(tsps);
                        continue;
                    }
                }

                debug!("VM is launched");
                tsps.launched = precise_time_ns();

                match vm.process_req(req.payload.clone()) {
                    Ok(rsp) => {
                        tsps.completed = precise_time_ns();
                        // TODO: output are currently ignored
                        debug!("{:?}", rsp);
                        vm_req_sender.send(Message::ReleaseVm(vm)).expect("Failed to send ReleaseVm request");
                        break RequestStatus::SentToVM(rsp);
                    }
                    Err(e) => {
                        handle_vm_error(e);
                        vm_req_sender.send(Message::DeleteVm(vm)).expect("Failed to send DeleteVm request");
                        // insert the request's timestamps
                        stat.push(tsps);
                        continue;
                    },
                }

            },
            Err(e) => {
                // If VM allocation fails it is an unrecoverable error, no point in retrying.
                let id = thread::current().id();
                break match e {
                    resource_manager::Error::InsufficientEvict |
                    resource_manager::Error::LowMemory(_) => {
                        error!("[Worker {:?}] Resource exhaustion", id);
                        RequestStatus::ResourceExhausted
                    }
                    resource_manager::Error::FunctionNotExist=> {
                        error!("[Worker {:?}] Requested function doesn't exist: {:?}", id, function_name);
                        RequestStatus::FunctionNotExist
                    }
                    _ => {
                        error!("[Worker {:?}] Unexpected resource_manager error: {:?}", id, e);
                        RequestStatus::Dropped
                    }
                };
            }
        }
    };

    // insert the request's timestamps
    stat.push(tsps);
    Response { status: result }
}

impl Worker {
    pub fn new(
        sched_addr: String,
        vm_req_sender: Sender<Message>,
        cid: u32,
    ) -> Self {
        let handle = thread::spawn(move || {
            let id = thread::current().id();
            std::fs::create_dir_all("./out").unwrap();
            let log_file = std::fs::File::create(format!("./out/thread-{:?}.stat", id)).unwrap();
            let mut stat = metrics::WorkerMetrics::new(log_file);
            stat.start_timed_flush(FLUSH_INTERVAL_SECS);

            let vm_listener_path = format!("worker-{}.sock_1234", cid);
            let _ = std::fs::remove_file(&vm_listener_path);
            let vm_listener = match UnixListener::bind(vm_listener_path) {
                Ok(listener) => listener,
                Err(e) => panic!("Failed to bind to unix listener \"worker-{}.sock_1234\": {:?}", cid, e),
            };

            let sched_rpc = Arc::new(Mutex::new(Scheduler::new(sched_addr)));
            loop {
                let vm_listener_dup = match vm_listener.try_clone() {
                    Ok(listener) => listener,
                    Err(e) => panic!("Failed to clone unix listener \"worker-{}.sock_1234\": {:?}", cid, e),
                };

                let message = sched_rpc.lock().unwrap().get(); // wait for request
                let (req_id, req) = {
                    use sched::message::response::Kind;
                    use crate::request;
                    match message {
                        Ok(res) => {
                            match res.kind {
                                Some(Kind::ProcessTask(r)) => {
                                    let req = request::parse_u8_invoke(r.invoke)
                                                        .expect("Failed to parse request");
                                    (r.task_id, req)
                                }
                                Some(Kind::Terminate(_)) => {
                                    debug!("[Worker {:?}] terminate received", id);
                                    stat.flush();
                                    return;
                                }
                                _ => {
                                    error!("[Worker {:?}] Invalid response: {:?}", id, res);
                                    continue
                                }
                            }
                        }
                        Err(_) => {
                            error!("[Worker {:?}] Invalid message: {:?}", id, message);
                            continue
                        }
                    }
                };

                // FIXME dummy tsps fow now
                let dummy_tsps = RequestTimestamps {..Default::default()};
                let result = handle_request(req, Arc::clone(&sched_rpc),
                    vm_req_sender.clone(), vm_listener_dup, dummy_tsps, &mut stat, cid);

                let _ = sched_rpc.lock().unwrap().finish(req_id, result.to_vec()); // return the result
            }
        });

        Worker { thread: handle }
    }

    pub fn join(self) -> std::thread::Result<()> {
        self.thread.join()
    }
}

fn handle_vm_error(vme: vm::Error) {
    let id = thread::current().id();
    match vme {
        vm::Error::ProcessSpawn(_) | vm::Error::VsockListen(_) =>
            error!("[Worker {:?}] Failed to start vm due to: {:?}", id, vme),
        vm::Error::VsockRead(_) | vm::Error::VsockWrite(_) =>
            error!("[Worker {:?}] Vm failed to process request due to: {:?}", id, vme),
        _ => (),
    }
}
