// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use futures::{channel::mpsc, pin_mut, StreamExt};
use remain::sorted;
use thiserror::Error as ThisError;

use base::{self, error, info, warn, AsRawDescriptor, Event, RawDescriptor};
use cros_async::{select6, EventAsync, Executor};
use data_model::{DataInit, Le16, Le32, Le64};
use msg_socket::MsgSender;
use vm_control::{
    BalloonControlCommand, BalloonControlResponseSocket, BalloonControlResult, BalloonStats,
};
use vm_memory::{GuestAddress, GuestMemory};

use super::{
    copy_config, descriptor_utils, DescriptorChain, Interrupt, Queue, Reader, VirtioDevice,
    TYPE_BALLOON,
};

#[sorted]
#[derive(ThisError, Debug)]
pub enum BalloonError {
    /// Failed to create async message receiver.
    #[error("failed to create async message receiver: {0}")]
    CreatingMessageReceiver(msg_socket::MsgError),
    /// Failed to receive command message.
    #[error("failed to receive command message: {0}")]
    ReceivingCommand(msg_socket::MsgError),
    /// Failed to write config event.
    #[error("failed to write config event: {0}")]
    WritingConfigEvent(base::Error),
}
pub type Result<T> = std::result::Result<T, BalloonError>;

// Balloon has three virt IO queues: Inflate, Deflate, and Stats.
const QUEUE_SIZE: u16 = 128;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE, QUEUE_SIZE, QUEUE_SIZE];

const VIRTIO_BALLOON_PFN_SHIFT: u32 = 12;

// The feature bitmap for virtio balloon
const VIRTIO_BALLOON_F_MUST_TELL_HOST: u32 = 0; // Tell before reclaiming pages
const VIRTIO_BALLOON_F_STATS_VQ: u32 = 1; // Stats reporting enabled
const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u32 = 2; // Deflate balloon on OOM

// virtio_balloon_config is the balloon device configuration space defined by the virtio spec.
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
struct virtio_balloon_config {
    num_pages: Le32,
    actual: Le32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl DataInit for virtio_balloon_config {}

// BalloonConfig is modified by the worker and read from the device thread.
#[derive(Default)]
struct BalloonConfig {
    num_pages: AtomicUsize,
    actual_pages: AtomicUsize,
}

// The constants defining stats types in virtio_baloon_stat
const VIRTIO_BALLOON_S_SWAP_IN: u16 = 0;
const VIRTIO_BALLOON_S_SWAP_OUT: u16 = 1;
const VIRTIO_BALLOON_S_MAJFLT: u16 = 2;
const VIRTIO_BALLOON_S_MINFLT: u16 = 3;
const VIRTIO_BALLOON_S_MEMFREE: u16 = 4;
const VIRTIO_BALLOON_S_MEMTOT: u16 = 5;
const VIRTIO_BALLOON_S_AVAIL: u16 = 6;
const VIRTIO_BALLOON_S_CACHES: u16 = 7;
const VIRTIO_BALLOON_S_HTLB_PGALLOC: u16 = 8;
const VIRTIO_BALLOON_S_HTLB_PGFAIL: u16 = 9;

// BalloonStat is used to deserialize stats from the stats_queue.
#[derive(Copy, Clone)]
#[repr(C, packed)]
struct BalloonStat {
    tag: Le16,
    val: Le64,
}
// Safe because it only has data.
unsafe impl DataInit for BalloonStat {}

impl BalloonStat {
    fn update_stats(&self, stats: &mut BalloonStats) {
        let val = Some(self.val.to_native());
        match self.tag.to_native() {
            VIRTIO_BALLOON_S_SWAP_IN => stats.swap_in = val,
            VIRTIO_BALLOON_S_SWAP_OUT => stats.swap_out = val,
            VIRTIO_BALLOON_S_MAJFLT => stats.major_faults = val,
            VIRTIO_BALLOON_S_MINFLT => stats.minor_faults = val,
            VIRTIO_BALLOON_S_MEMFREE => stats.free_memory = val,
            VIRTIO_BALLOON_S_MEMTOT => stats.total_memory = val,
            VIRTIO_BALLOON_S_AVAIL => stats.available_memory = val,
            VIRTIO_BALLOON_S_CACHES => stats.disk_caches = val,
            VIRTIO_BALLOON_S_HTLB_PGALLOC => stats.hugetlb_allocations = val,
            VIRTIO_BALLOON_S_HTLB_PGFAIL => stats.hugetlb_failures = val,
            _ => (),
        }
    }
}

// Processes one message's list of addresses.
fn handle_address_chain<F>(
    avail_desc: DescriptorChain,
    mem: &GuestMemory,
    desc_handler: &mut F,
) -> descriptor_utils::Result<()>
where
    F: FnMut(GuestAddress),
{
    let mut reader = Reader::new(mem.clone(), avail_desc)?;
    for res in reader.iter::<Le32>() {
        let pfn = match res {
            Ok(pfn) => pfn,
            Err(e) => {
                error!("error while reading unused pages: {}", e);
                break;
            }
        };
        let guest_address = GuestAddress((u64::from(pfn.to_native())) << VIRTIO_BALLOON_PFN_SHIFT);

        desc_handler(guest_address);
    }
    Ok(())
}

// Async task that handles the main balloon inflate and deflate queues.
async fn handle_queue<F>(
    mem: &GuestMemory,
    mut queue: Queue,
    mut queue_event: EventAsync,
    interrupt: Rc<RefCell<Interrupt>>,
    mut desc_handler: F,
) where
    F: FnMut(GuestAddress),
{
    loop {
        let avail_desc = match queue.next_async(mem, &mut queue_event).await {
            Err(e) => {
                error!("Failed to read descriptor {}", e);
                return;
            }
            Ok(d) => d,
        };
        let index = avail_desc.index;
        if let Err(e) = handle_address_chain(avail_desc, mem, &mut desc_handler) {
            error!("balloon: failed to process inflate addresses: {}", e);
        }
        queue.add_used(mem, index, 0);
        interrupt.borrow_mut().signal_used_queue(queue.vector);
    }
}

// Async task that handles the stats queue. Note that the cadence of this is driven by requests for
// balloon stats from the control pipe.
// The guests queues an initial buffer on boot, which is read and then this future will block until
// signaled from the command socket that stats should be collected again.
async fn handle_stats_queue(
    mem: &GuestMemory,
    mut queue: Queue,
    mut queue_event: EventAsync,
    mut stats_rx: mpsc::Receiver<()>,
    command_socket: &BalloonControlResponseSocket,
    config: Arc<BalloonConfig>,
    interrupt: Rc<RefCell<Interrupt>>,
) {
    loop {
        let stats_desc = match queue.next_async(mem, &mut queue_event).await {
            Err(e) => {
                error!("Failed to read descriptor {}", e);
                return;
            }
            Ok(d) => d,
        };
        let index = stats_desc.index;
        let mut reader = match Reader::new(mem.clone(), stats_desc) {
            Ok(r) => r,
            Err(e) => {
                error!("balloon: failed to CREATE Reader: {}", e);
                continue;
            }
        };
        let mut stats: BalloonStats = Default::default();
        for res in reader.iter::<BalloonStat>() {
            match res {
                Ok(stat) => stat.update_stats(&mut stats),
                Err(e) => {
                    error!("error while reading stats: {}", e);
                    break;
                }
            };
        }
        let actual_pages = config.actual_pages.load(Ordering::Relaxed) as u64;
        let result = BalloonControlResult::Stats {
            balloon_actual: actual_pages << VIRTIO_BALLOON_PFN_SHIFT,
            stats,
        };
        if let Err(e) = command_socket.send(&result) {
            error!("failed to send stats result: {}", e);
        }

        // Wait for a request to read the stats again.
        if stats_rx.next().await.is_none() {
            error!("stats signal channel was closed");
            break;
        }

        // Request a new stats_desc to the guest.
        queue.add_used(&mem, index, 0);
        interrupt.borrow_mut().signal_used_queue(queue.vector);
    }
}

// Async task that handles the command socket. The command socket handles messages from the host
// requesting that the guest balloon be adjusted or to report guest memory statistics.
async fn handle_command_socket(
    ex: &Executor,
    command_socket: &BalloonControlResponseSocket,
    interrupt: Rc<RefCell<Interrupt>>,
    config: Arc<BalloonConfig>,
    mut stats_tx: mpsc::Sender<()>,
) -> Result<()> {
    let mut async_messages = command_socket
        .async_receiver(ex)
        .map_err(BalloonError::CreatingMessageReceiver)?;
    loop {
        match async_messages.next().await {
            Ok(command) => match command {
                BalloonControlCommand::Adjust { num_bytes } => {
                    let num_pages = (num_bytes >> VIRTIO_BALLOON_PFN_SHIFT) as usize;
                    info!("balloon config changed to consume {} pages", num_pages);

                    config.num_pages.store(num_pages, Ordering::Relaxed);
                    interrupt.borrow_mut().signal_config_changed();
                }
                BalloonControlCommand::Stats => {
                    if let Err(e) = stats_tx.try_send(()) {
                        error!("failed to signal the stat handler: {}", e);
                    }
                }
            },
            Err(e) => {
                return Err(BalloonError::ReceivingCommand(e));
            }
        }
    }
}

// Async task that resamples the status of the interrupt when the guest sends a request by
// signalling the resample event associated with the interrupt.
async fn handle_irq_resample(ex: &Executor, interrupt: Rc<RefCell<Interrupt>>) {
    let resample_evt = interrupt
        .borrow_mut()
        .get_resample_evt()
        .try_clone()
        .unwrap();
    let resample_evt = EventAsync::new(resample_evt.0, ex).unwrap();
    while resample_evt.next_val().await.is_ok() {
        interrupt.borrow_mut().do_interrupt_resample();
    }
}

// Async task that waits for a signal from the kill event given to the device at startup.  Once this event is
// readable, exit. Exiting this future will cause the main loop to break and the worker thread to
// exit.
async fn wait_kill(kill_evt: EventAsync) {
    let _ = kill_evt.next_val().await;
}

// The main worker thread. Initialized the asynchronous worker tasks and passes them to the executor
// to be processed.
fn run_worker(
    mut queue_evts: Vec<Event>,
    mut queues: Vec<Queue>,
    command_socket: &BalloonControlResponseSocket,
    interrupt: Interrupt,
    kill_evt: Event,
    mem: GuestMemory,
    config: Arc<BalloonConfig>,
) {
    // Wrap the interrupt in a `RefCell` so it can be shared between async functions.
    let interrupt = Rc::new(RefCell::new(interrupt));

    let ex = Executor::new().unwrap();

    // The first queue is used for inflate messages
    let inflate_event =
        EventAsync::new(queue_evts.remove(0).0, &ex).expect("failed to set up the inflate event");
    let inflate = handle_queue(
        &mem,
        queues.remove(0),
        inflate_event,
        interrupt.clone(),
        |guest_address| {
            if let Err(e) = mem.remove_range(guest_address, 1 << VIRTIO_BALLOON_PFN_SHIFT) {
                warn!("Marking pages unused failed: {}, addr={}", e, guest_address);
            }
        },
    );
    pin_mut!(inflate);

    // The second queue is used for deflate messages
    let deflate_event =
        EventAsync::new(queue_evts.remove(0).0, &ex).expect("failed to set up the deflate event");
    let deflate = handle_queue(
        &mem,
        queues.remove(0),
        deflate_event,
        interrupt.clone(),
        std::mem::drop, // Ignore these.
    );
    pin_mut!(deflate);

    // The third queue is used for stats messages
    let (stats_tx, stats_rx) = mpsc::channel::<()>(1);
    let stats_event =
        EventAsync::new(queue_evts.remove(0).0, &ex).expect("failed to set up the stats event");
    let stats = handle_stats_queue(
        &mem,
        queues.remove(0),
        stats_event,
        stats_rx,
        command_socket,
        config.clone(),
        interrupt.clone(),
    );
    pin_mut!(stats);

    // Future to handle command messages that resize the balloon.
    let command = handle_command_socket(&ex, command_socket, interrupt.clone(), config, stats_tx);
    pin_mut!(command);

    // Process any requests to resample the irq value.
    let resample = handle_irq_resample(&ex, interrupt.clone());
    pin_mut!(resample);

    // Exit if the kill event is triggered.
    let kill_evt = EventAsync::new(kill_evt.0, &ex).expect("failed to set up the kill event");
    let kill = wait_kill(kill_evt);
    pin_mut!(kill);

    if let Err(e) = ex.run_until(select6(inflate, deflate, stats, command, resample, kill)) {
        error!("error happened in executor: {}", e);
    }
}

/// Virtio device for memory balloon inflation/deflation.
pub struct Balloon {
    command_socket: Option<BalloonControlResponseSocket>,
    config: Arc<BalloonConfig>,
    features: u64,
    kill_evt: Option<Event>,
    worker_thread: Option<thread::JoinHandle<BalloonControlResponseSocket>>,
}

impl Balloon {
    /// Creates a new virtio balloon device.
    pub fn new(
        base_features: u64,
        command_socket: BalloonControlResponseSocket,
    ) -> Result<Balloon> {
        Ok(Balloon {
            command_socket: Some(command_socket),
            config: Arc::new(BalloonConfig {
                num_pages: AtomicUsize::new(0),
                actual_pages: AtomicUsize::new(0),
            }),
            kill_evt: None,
            worker_thread: None,
            features: base_features
                | 1 << VIRTIO_BALLOON_F_MUST_TELL_HOST
                | 1 << VIRTIO_BALLOON_F_STATS_VQ
                | 1 << VIRTIO_BALLOON_F_DEFLATE_ON_OOM,
        })
    }

    fn get_config(&self) -> virtio_balloon_config {
        let num_pages = self.config.num_pages.load(Ordering::Relaxed) as u32;
        let actual_pages = self.config.actual_pages.load(Ordering::Relaxed) as u32;
        virtio_balloon_config {
            num_pages: num_pages.into(),
            actual: actual_pages.into(),
        }
    }
}

impl Drop for Balloon {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do with a failure.
            let _ = kill_evt.write(1);
        }

        if let Some(worker_thread) = self.worker_thread.take() {
            let _ = worker_thread.join();
        }
    }
}

impl VirtioDevice for Balloon {
    fn keep_rds(&self) -> Vec<RawDescriptor> {
        vec![self.command_socket.as_ref().unwrap().as_raw_descriptor()]
    }

    fn device_type(&self) -> u32 {
        TYPE_BALLOON
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        copy_config(data, 0, self.get_config().as_slice(), offset);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let mut config = self.get_config();
        copy_config(config.as_mut_slice(), offset, data, 0);
        self.config
            .actual_pages
            .store(config.actual.to_native() as usize, Ordering::Relaxed);
    }

    fn features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, value: u64) {
        self.features &= value;
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Interrupt,
        queues: Vec<Queue>,
        queue_evts: Vec<Event>,
    ) {
        if queues.len() != QUEUE_SIZES.len() || queue_evts.len() != QUEUE_SIZES.len() {
            return;
        }

        let (self_kill_evt, kill_evt) = match Event::new().and_then(|e| Ok((e.try_clone()?, e))) {
            Ok(v) => v,
            Err(e) => {
                error!("failed to create kill Event pair: {}", e);
                return;
            }
        };
        self.kill_evt = Some(self_kill_evt);

        let config = self.config.clone();
        let command_socket = self.command_socket.take().unwrap();
        let worker_result = thread::Builder::new()
            .name("virtio_balloon".to_string())
            .spawn(move || {
                run_worker(
                    queue_evts,
                    queues,
                    &command_socket,
                    interrupt,
                    kill_evt,
                    mem,
                    config,
                );
                command_socket // Return the command socket so it can be re-used.
            });

        match worker_result {
            Err(e) => {
                error!("failed to spawn virtio_balloon worker: {}", e);
            }
            Ok(join_handle) => {
                self.worker_thread = Some(join_handle);
            }
        }
    }

    fn reset(&mut self) -> bool {
        if let Some(kill_evt) = self.kill_evt.take() {
            if kill_evt.write(1).is_err() {
                error!("{}: failed to notify the kill event", self.debug_label());
                return false;
            }
        }

        if let Some(worker_thread) = self.worker_thread.take() {
            match worker_thread.join() {
                Err(_) => {
                    error!("{}: failed to get back resources", self.debug_label());
                    return false;
                }
                Ok(command_socket) => {
                    self.command_socket = Some(command_socket);
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::virtio::descriptor_utils::{create_descriptor_chain, DescriptorType};

    #[test]
    fn desc_parsing_inflate() {
        // Check that the memory addresses are parsed correctly by 'handle_address_chain' and passed
        // to the closure.
        let memory_start_addr = GuestAddress(0x0);
        let memory = GuestMemory::new(&vec![(memory_start_addr, 0x10000)]).unwrap();
        memory
            .write_obj_at_addr(0x10u32, GuestAddress(0x100))
            .unwrap();
        memory
            .write_obj_at_addr(0xaa55aa55u32, GuestAddress(0x104))
            .unwrap();

        let chain = create_descriptor_chain(
            &memory,
            GuestAddress(0x0),
            GuestAddress(0x100),
            vec![(DescriptorType::Readable, 8)],
            0,
        )
        .expect("create_descriptor_chain failed");

        let mut addrs = Vec::new();
        let res = handle_address_chain(chain, &memory, &mut |guest_address| {
            addrs.push(guest_address);
        });
        assert!(res.is_ok());
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], GuestAddress(0x10u64 << VIRTIO_BALLOON_PFN_SHIFT));
        assert_eq!(
            addrs[1],
            GuestAddress(0xaa55aa55u64 << VIRTIO_BALLOON_PFN_SHIFT)
        );
    }
}
