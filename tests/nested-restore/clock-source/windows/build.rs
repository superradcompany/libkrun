fn main() {
    for name in [
        "CancelRunVirtualProcessor",
        "CreatePartition",
        "CreateVirtualProcessor",
        "DeletePartition",
        "DeleteVirtualProcessor",
        "GetPartitionProperty",
        "GetVirtualProcessorRegisters",
        "GetVirtualProcessorState",
        "MapGpaRange",
        "QueryGpaRangeDirtyBitmap",
        "RequestInterrupt",
        "ResumePartitionTime",
        "RunVirtualProcessor",
        "SetPartitionProperty",
        "SetVirtualProcessorRegisters",
        "SetVirtualProcessorState",
        "SuspendPartitionTime",
        "TranslateGva",
        "UnmapGpaRange",
    ] {
        println!("cargo:rustc-cdylib-link-arg=/export:WHv{name}=WinHvPlatformNative.WHv{name}");
    }
}
