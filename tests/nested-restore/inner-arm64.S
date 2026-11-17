// EL1 inner VM: increment RAM while servicing virtual timer PPI 27 at 100 Hz.
// RAM starts at 0x40000000; progress and fatal-exception words start at +0x4000.
.text
.balign 2048
.global inner_start, inner_end
inner_start:
    // Enable the virtual GIC distributor and this CPU's group-1 timer interrupt.
    mov x0, #0x8000000
    mov w1, #0x12
    str w1, [x0]
    mov x0, #0x80a0000
    str wzr, [x0, #0x14]
    add x0, x0, #0x10000
    mov w1, #0x8000000
    str w1, [x0, #0x80]
    str w1, [x0, #0x100]
    mov x1, #1
    msr icc_sre_el1, x1
    isb
    mov x1, #255
    msr icc_pmr_el1, x1
    mov x1, #1
    msr icc_igrpen1_el1, x1
    adr x1, vectors
    msr vbar_el1, x1
    mrs x1, cntfrq_el0
    mov x2, #100
    udiv x1, x1, x2
    msr cntv_tval_el0, x1
    mov x1, #1
    msr cntv_ctl_el0, x1
    isb
    dsb sy
    msr daifclr, #2
    mov x0, #0x40000000
    add x0, x0, #0x4000
1:  ldr w1, [x0]
    add w1, w1, #1
    str w1, [x0]
    b 1b
.balign 2048
vectors:
.rept 4
    b fatal
    .balign 128
    b irq
    .balign 128
    b fatal
    .balign 128
    b fatal
    .balign 128
.endr
irq:
    stp x0, x1, [sp, #-32]!
    stp x2, x3, [sp, #16]
    mrs x3, icc_iar1_el1
    cmp x3, #27
    b.ne ack
    mov x0, #0x40000000
    add x0, x0, #0x4000
    ldr w1, [x0, #4]
    add w1, w1, #1
    str w1, [x0, #4]
    mrs x1, cntfrq_el0
    mov x2, #100
    udiv x1, x1, x2
    msr cntv_tval_el0, x1
    isb
ack:
    msr icc_eoir1_el1, x3
    ldp x2, x3, [sp, #16]
    ldp x0, x1, [sp], #32
    eret
fatal:
    mov x0, #0x40000000
    add x0, x0, #0x4000
    mov w1, #1
    str w1, [x0, #8]
    b fatal
inner_end:
