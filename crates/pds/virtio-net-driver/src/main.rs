#![no_std]
#![no_main]
#![feature(never_type)]
#![feature(let_chains)]

use core::ptr::NonNull;

use virtio_drivers::{
    device::net::*,
    transport::{
        mmio::{MmioTransport, VirtIOHeader},
        DeviceType, Transport,
    },
};

use sel4_externally_shared::ExternallySharedRef;
use sel4_microkit::{memory_region_symbol, protection_domain, var, Channel, Handler, MessageInfo};
use sel4_microkit_message::MessageInfoExt as _;
use sel4_shared_ring_buffer::{RingBuffer, RingBuffers};

use microkit_http_server_example_virtio_hal_impl::HalImpl;
use microkit_http_server_example_virtio_net_driver_interface_types::*;

use smoltcp::phy::{self, RxToken, TxToken};
use smoltcp::time::Instant;

const DEVICE: Channel = Channel::new(0);
const CLIENT: Channel = Channel::new(1);

const NET_QUEUE_SIZE: usize = 16;
const NET_BUFFER_LEN: usize = 2048;

#[protection_domain(
    heap_size = 512 * 1024,
)]
fn init() -> PhyDeviceHandler<virtio_phy::Device> {
    HalImpl::init(
        *var!(virtio_net_driver_dma_size: usize = 0),
        *var!(virtio_net_driver_dma_vaddr: usize = 0),
        *var!(virtio_net_driver_dma_paddr: usize = 0),
    );

    let mut dev = {
        let header = NonNull::new(
            (*var!(virtio_net_mmio_vaddr: usize = 0) + *var!(virtio_net_mmio_offset: usize = 0))
                as *mut VirtIOHeader,
        )
        .unwrap();
        let transport = unsafe { MmioTransport::new(header) }.unwrap();
        assert_eq!(transport.device_type(), DeviceType::Network);
        virtio_phy::Device::new(
            VirtIONet::<HalImpl, MmioTransport, NET_QUEUE_SIZE>::new(transport, NET_BUFFER_LEN)
                .unwrap()
        )
    };

    let client_region = unsafe {
        ExternallySharedRef::<'static, _>::new(
            memory_region_symbol!(virtio_net_client_dma_vaddr: *mut [u8], n = *var!(virtio_net_client_dma_size: usize = 0)),
        )
    };

    let client_region_paddr = *var!(virtio_net_client_dma_paddr: usize = 0);

    let rx_ring_buffers = unsafe {
        RingBuffers::<'_, fn() -> Result<(), !>>::new(
            RingBuffer::from_ptr(memory_region_symbol!(virtio_net_rx_free: *mut _)),
            RingBuffer::from_ptr(memory_region_symbol!(virtio_net_rx_used: *mut _)),
            notify_client,
            true,
        )
    };

    let tx_ring_buffers = unsafe {
        RingBuffers::<'_, fn() -> Result<(), !>>::new(
            RingBuffer::from_ptr(memory_region_symbol!(virtio_net_tx_free: *mut _)),
            RingBuffer::from_ptr(memory_region_symbol!(virtio_net_tx_used: *mut _)),
            notify_client,
            true,
        )
    };

    dev.irq_ack();
    DEVICE.irq_ack().unwrap();

    PhyDeviceHandler::new(
        dev,
        client_region,
        client_region_paddr,
        rx_ring_buffers,
        tx_ring_buffers,
    )
}

mod virtio_phy {
    use super::*;

    extern crate alloc;
    use alloc::rc::Rc;
    use core::cell::RefCell;

    use virtio_drivers::device::net::{self, RxBuffer};

    use smoltcp::phy;
    use smoltcp::time::Instant;

    pub type VirtIONet = net::VirtIONet<HalImpl, MmioTransport, {NET_QUEUE_SIZE}>;

    pub struct RxToken {
        dev_inner: Rc<RefCell<VirtIONet>>,
        // NOTE This is an option so we can call mem::take on it for implementing
        // Drop. This is necessary because virtio's deallocation function moves
        // its argument, which we can't normally do in Drop::drop.
        buf: Option<RxBuffer>,
    }

    impl phy::RxToken for RxToken {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, f: F) -> R {
            // XXX: Why is this mut? We could avoid calling packet_mut by
            // creating a temporary vector, but this would add a copy.
            f(self.buf.as_mut().unwrap().packet_mut())
        }
    }

    impl Drop for RxToken {
        fn drop(&mut self) {
            self.dev_inner.borrow_mut().recycle_rx_buffer(self.buf.take().unwrap());
        }
    }

    pub struct TxToken {
        dev_inner: Rc<RefCell<VirtIONet>>,
    }

    impl phy::TxToken for TxToken {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            let mut dev = self.dev_inner.borrow_mut();

            let mut buf = dev.new_tx_buffer(len);
            let res = f(buf.packet_mut());
            // XXX How can we avoid panicking here? Appears to fail only if the
            // queue is full.
            dev.send(buf).expect("Failed to send buffer");

            res
        }
    }

    pub struct Device {
        dev_inner: Rc<RefCell<VirtIONet>>,
    }

    impl Device {
        pub fn new(dev: VirtIONet) -> Self {
            Self {
                dev_inner: Rc::new(RefCell::new(dev)),
            }
        }
    }

    impl phy::Device for Device {
        type RxToken<'a> = RxToken;
        type TxToken<'a> = TxToken;

        fn receive(
            &mut self,
            _timestamp: Instant,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            let mut dev = self.dev_inner.borrow_mut();

            if !dev.can_recv() || !dev.can_send() {
                return None;
            }

            let rx_buf = dev.receive().ok()?;

            Some((
                RxToken {
                    dev_inner: self.dev_inner.clone(),
                    buf: Some(rx_buf),
                },
                TxToken {
                    dev_inner: self.dev_inner.clone(),
                },
            ))
        }

        fn transmit(
            &mut self,
            _timestamp: Instant,
        ) -> Option<Self::TxToken<'_>> {
            if !self.dev_inner.borrow().can_send() {
                return None;
            }

            Some(TxToken {
                dev_inner: self.dev_inner.clone(),
            })
        }

        fn capabilities(&self) -> phy::DeviceCapabilities {
            // XXX What are these for virtio?
            phy::DeviceCapabilities::default()
        }
    }

    impl IrqAck for Device {
        fn irq_ack(&mut self) {
            self.dev_inner.borrow_mut().ack_interrupt();
        }
    }

    impl HasMac for Device {
        fn mac_address(&self) -> [u8; 6] {
            self.dev_inner.borrow().mac_address()
        }
    }
}

pub trait IrqAck {
    fn irq_ack(&mut self);
}

pub trait HasMac {
    fn mac_address(&self) -> [u8; 6];
}

// XXX How do we attach this to a a token in another module. Notice, no self
// argument...
fn notify_client() -> Result<(), !> {
    CLIENT.notify();
    Ok::<_, !>(())
}

struct PhyDeviceHandler<Device> {
    dev: Device,
    client_region: ExternallySharedRef<'static, [u8]>,
    client_region_paddr: usize,
    rx_ring_buffers: RingBuffers<'static, fn() -> Result<(), !>>,
    tx_ring_buffers: RingBuffers<'static, fn() -> Result<(), !>>,
}

impl<Device> PhyDeviceHandler<Device> {
    pub fn new(
        dev: Device,
        client_region: ExternallySharedRef<'static, [u8]>,
        client_region_paddr: usize,
        rx_ring_buffers: RingBuffers<'static, fn() -> Result<(), !>>,
        tx_ring_buffers: RingBuffers<'static, fn() -> Result<(), !>>,
    ) -> Self {
        // XXX We could maybe initialize DMA here, so we don't need to do
        // it in main. Also maybe initialize the ring buffers.
        Self {
            dev,
            client_region,
            client_region_paddr,
            rx_ring_buffers,
            tx_ring_buffers,
        }
    }
}

impl<Device: phy::Device + IrqAck + HasMac> Handler for PhyDeviceHandler<Device> {
    type Error = !;

    fn notified(&mut self, channel: Channel) -> Result<(), Self::Error> {
        match channel {
            DEVICE | CLIENT => {
                let mut notify_rx = false;

                while !self.rx_ring_buffers.free().is_empty()
                    && let Some((rx_tok, _tx_tok)) = self.dev.receive(Instant::ZERO) {
                        let desc = self.rx_ring_buffers.free_mut().dequeue().unwrap();
                        let desc_len = usize::try_from(desc.len()).unwrap();

                        rx_tok.consume(|rx_buf| {
                            assert!(desc_len >= rx_buf.len());
                            let buf_range = {
                                let start = desc.encoded_addr() - self.client_region_paddr;
                                start..start + rx_buf.len()
                            };
                            self.client_region
                                .as_mut_ptr()
                                .index(buf_range)
                                .copy_from_slice(&rx_buf);
                        });

                        self.rx_ring_buffers.used_mut().enqueue(desc).unwrap();
                        notify_rx = true;
                    }

                if notify_rx {
                    self.rx_ring_buffers.notify().unwrap();
                }

                let mut notify_tx = false;

                while !self.tx_ring_buffers.free().is_empty()
                    && let Some(tx_tok) = self.dev.transmit(Instant::ZERO) {
                        let desc = self.tx_ring_buffers.free_mut().dequeue().unwrap();
                        let tx_len = usize::try_from(desc.len()).unwrap();

                        tx_tok.consume(tx_len, |tx_buf| {
                            let buf_range = {
                                let start = desc.encoded_addr() - self.client_region_paddr;
                                start..start + tx_len
                            };
                            self.client_region
                                .as_ptr()
                                .index(buf_range)
                                .copy_into_slice(tx_buf);
                        });

                        self.tx_ring_buffers.used_mut().enqueue(desc).unwrap();
                        notify_tx = true;
                    }

                if notify_tx {
                    self.tx_ring_buffers.notify().unwrap();
                }

                self.dev.irq_ack();
                DEVICE.irq_ack().unwrap();
            }
            _ => {
                unreachable!()
            }
        }
        Ok(())
    }

    fn protected(
        &mut self,
        channel: Channel,
        msg_info: MessageInfo,
    ) -> Result<MessageInfo, Self::Error> {
        Ok(match channel {
            CLIENT => match msg_info.recv_using_postcard::<Request>() {
                Ok(req) => match req {
                    Request::GetMacAddress => {
                        let mac_address = self.dev.mac_address();
                        MessageInfo::send_using_postcard(GetMacAddressResponse {
                            mac_address: MacAddress(mac_address),
                        })
                        .unwrap()
                    }
                },
                Err(_) => MessageInfo::send_unspecified_error(),
            },
            _ => {
                unreachable!()
            }
        })
    }
}
