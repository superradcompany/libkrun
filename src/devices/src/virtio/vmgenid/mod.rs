mod device;
mod event_handler;

pub use self::defs::uapi::VIRTIO_ID_MSB_VMGENID as TYPE_MSB_VMGENID;
pub use self::device::{
    Generation, GenerationId, GenerationProcessingHandle, GenerationStateSnapshot,
    GenerationWaitOutcome,
};

mod defs {
    use super::super::QueueConfig;

    pub const VMGENID_DEV_ID: &str = "virtio_msb_vmgenid";
    pub const NUM_QUEUES: usize = 1;
    pub const QUEUE_SIZE: u16 = 1;
    pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];

    pub mod uapi {
        pub const VIRTIO_F_VERSION_1: u32 = 32;
        pub const VIRTIO_ID_MSB_VMGENID: u32 = 0x4d47;
    }
}

#[derive(Debug)]
pub enum GenerationDeviceError {
    /// Failed to create the activation event.
    EventFd(std::io::Error),
}

type Result<T> = std::result::Result<T, GenerationDeviceError>;
