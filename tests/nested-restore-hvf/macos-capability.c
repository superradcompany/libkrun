#include <Hypervisor/Hypervisor.h>
#include <stdio.h>
int main(void) {
    bool supported = false;
    hv_return_t result = hv_vm_config_get_el2_supported(&supported);
    printf("EL2 capability: result=0x%x supported=%d\n", result, supported);
    return result != HV_SUCCESS || !supported;
}
