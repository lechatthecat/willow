00000000000161f8 <_willow_root_depth>:
   161f8: a9be4ff4     	stp	x20, x19, [sp, #-0x20]!
   161fc: a9017bfd     	stp	x29, x30, [sp, #0x10]
   16200: 910043fd     	add	x29, sp, #0x10
   16204: 90000000     	adrp	x0, 0x16000 <_willow_pop_root+0xc8>
   16208: f9400000     	ldr	x0, [x0]
   1620c: f9400008     	ldr	x8, [x0]
   16210: d63f0100     	blr	x8
   16214: aa0003f3     	mov	x19, x0
   16218: 39408008     	ldrb	w8, [x0, #0x20]
   1621c: 7100051f     	cmp	w8, #0x1
   16220: 540001a0     	b.eq	0x16254 <_willow_root_depth+0x5c>
   16224: 7100091f     	cmp	w8, #0x2
   16228: 540000a1     	b.ne	0x1623c <_willow_root_depth+0x44>
   1622c: 90000000     	adrp	x0, 0x16000 <_willow_pop_root+0xc8>
   16230: 91000000     	add	x0, x0, #0x0
   16234: 94000000     	bl	0x16234 <_willow_root_depth+0x3c>
   16238: 1400001d     	b	0x162ac <_willow_root_depth+0xb4>
   1623c: 90000001     	adrp	x1, 0x16000 <_willow_pop_root+0xc8>
   16240: 91000021     	add	x1, x1, #0x0
   16244: aa1303e0     	mov	x0, x19
   16248: 94000000     	bl	0x16248 <_willow_root_depth+0x50>
   1624c: 52800028     	mov	w8, #0x1                ; =1
   16250: 39008268     	strb	w8, [x19, #0x20]
   16254: f9400268     	ldr	x8, [x19]
   16258: 92f00009     	mov	x9, #0x7fffffffffffffff ; =9223372036854775807
   1625c: eb09011f     	cmp	x8, x9
   16260: 54000142     	b.hs	0x16288 <_willow_root_depth+0x90>
   16264: 91000509     	add	x9, x8, #0x1
   16268: f9000269     	str	x9, [x19]
   1626c: f9400e60     	ldr	x0, [x19, #0x18]
   16270: d35ffc09     	lsr	x9, x0, #31
   16274: b5000129     	cbnz	x9, 0x16298 <_willow_root_depth+0xa0>
   16278: f9000268     	str	x8, [x19]
   1627c: a9417bfd     	ldp	x29, x30, [sp, #0x10]
   16280: a8c24ff4     	ldp	x20, x19, [sp], #0x20
   16284: d65f03c0     	ret
   16288: 90000000     	adrp	x0, 0x16000 <_willow_pop_root+0xc8>
   1628c: 91000000     	add	x0, x0, #0x0
   16290: 94000000     	bl	0x16290 <_willow_root_depth+0x98>
   16294: 14000006     	b	0x162ac <_willow_root_depth+0xb4>
   16298: 90000000     	adrp	x0, 0x16000 <_willow_pop_root+0xc8>
   1629c: 91000000     	add	x0, x0, #0x0
   162a0: 52800ca1     	mov	w1, #0x65               ; =101
   162a4: 94000000     	bl	0x162a4 <_willow_root_depth+0xac>
   162a8: 94000000     	bl	0x162a8 <_willow_root_depth+0xb0>
   162ac: d4200020     	brk	#0x1
   162b0: f9400268     	ldr	x8, [x19]
   162b4: d1000508     	sub	x8, x8, #0x1
   162b8: f9000268     	str	x8, [x19]
   162bc: 94000000     	bl	0x162bc <_willow_root_depth+0xc4>
   162c0: 94000000     	bl	0x162c0 <_willow_root_depth+0xc8>

0000000000004ae8 <_willow_panic_depth>:
    4ae8: d10103ff     	sub	sp, sp, #0x40
    4aec: a90157f6     	stp	x22, x21, [sp, #0x10]
    4af0: a9024ff4     	stp	x20, x19, [sp, #0x20]
    4af4: a9037bfd     	stp	x29, x30, [sp, #0x30]
    4af8: 9100c3fd     	add	x29, sp, #0x30
    4afc: 90000000     	adrp	x0, 0x4000 <__ZN4core3ptr94drop_in_place$LT$willow_runtime..stack_trace..replace_current..$u7b$$u7b$closure$u7d$$u7d$$GT$17h97a9ff60e65db6c5E.llvm.16627454700614441515+0x48>
    4b00: f9400000     	ldr	x0, [x0]
    4b04: f9400008     	ldr	x8, [x0]
    4b08: d63f0100     	blr	x8
    4b0c: 39404008     	ldrb	w8, [x0, #0x10]
    4b10: 7100051f     	cmp	w8, #0x1
    4b14: 540001c0     	b.eq	0x4b4c <_willow_panic_depth+0x64>
    4b18: 7100091f     	cmp	w8, #0x2
    4b1c: 540000a1     	b.ne	0x4b30 <_willow_panic_depth+0x48>
    4b20: 90000000     	adrp	x0, 0x4000 <__ZN4core3ptr94drop_in_place$LT$willow_runtime..stack_trace..replace_current..$u7b$$u7b$closure$u7d$$u7d$$GT$17h97a9ff60e65db6c5E.llvm.16627454700614441515+0x48>
    4b24: 91000000     	add	x0, x0, #0x0
    4b28: 94000000     	bl	0x4b28 <_willow_panic_depth+0x40>
    4b2c: 1400003c     	b	0x4c1c <_willow_panic_depth+0x134>
    4b30: 90000001     	adrp	x1, 0x4000 <__ZN4core3ptr94drop_in_place$LT$willow_runtime..stack_trace..replace_current..$u7b$$u7b$closure$u7d$$u7d$$GT$17h97a9ff60e65db6c5E.llvm.16627454700614441515+0x48>
    4b34: 91000021     	add	x1, x1, #0x0
    4b38: aa0003f3     	mov	x19, x0
    4b3c: 94000000     	bl	0x4b3c <_willow_panic_depth+0x54>
    4b40: 52800028     	mov	w8, #0x1                ; =1
    4b44: aa1303e0     	mov	x0, x19
    4b48: 39004268     	strb	w8, [x19, #0x10]
    4b4c: f9400009     	ldr	x9, [x0]
    4b50: 92f00008     	mov	x8, #0x7fffffffffffffff ; =9223372036854775807
    4b54: eb08013f     	cmp	x9, x8
    4b58: 540005c2     	b.hs	0x4c10 <_willow_panic_depth+0x128>
    4b5c: 91000528     	add	x8, x9, #0x1
    4b60: f9000008     	str	x8, [x0]
    4b64: f9400408     	ldr	x8, [x0, #0x8]
    4b68: b4000468     	cbz	x8, 0x4bf4 <_willow_panic_depth+0x10c>
    4b6c: 52800029     	mov	w9, #0x1                ; =1
    4b70: f8290108     	ldadd	x9, x8, [x8]
    4b74: b7f80548     	tbnz	x8, #0x3f, 0x4c1c <_willow_panic_depth+0x134>
    4b78: a9404c08     	ldp	x8, x19, [x0]
    4b7c: d1000508     	sub	x8, x8, #0x1
    4b80: f9000008     	str	x8, [x0]
    4b84: f90007f3     	str	x19, [sp, #0x8]
    4b88: d9418260     	ldapur	x0, [x19, #0x18]
    4b8c: b40004a0     	cbz	x0, 0x4c20 <_willow_panic_depth+0x138>
    4b90: 94000000     	bl	0x4b90 <_willow_panic_depth+0xa8>
    4b94: 90000015     	adrp	x21, 0x4000 <__ZN4core3ptr94drop_in_place$LT$willow_runtime..stack_trace..replace_current..$u7b$$u7b$closure$u7d$$u7d$$GT$17h97a9ff60e65db6c5E.llvm.16627454700614441515+0x48>
    4b98: f94002b5     	ldr	x21, [x21]
    4b9c: f94002a8     	ldr	x8, [x21]
    4ba0: f240f91f     	tst	x8, #0x7fffffffffffffff
    4ba4: 54000441     	b.ne	0x4c2c <_willow_panic_depth+0x144>
    4ba8: 3940827f     	ldrb	wzr, [x19, #0x20]
    4bac: f9401e74     	ldr	x20, [x19, #0x38]
    4bb0: f94002a8     	ldr	x8, [x21]
    4bb4: f240f91f     	tst	x8, #0x7fffffffffffffff
    4bb8: 54000441     	b.ne	0x4c40 <_willow_panic_depth+0x158>
    4bbc: f8418260     	ldur	x0, [x19, #0x18]
    4bc0: 94000000     	bl	0x4bc0 <_willow_panic_depth+0xd8>
    4bc4: f94007e8     	ldr	x8, [sp, #0x8]
    4bc8: 92800009     	mov	x9, #-0x1               ; =-1
    4bcc: f8690108     	ldaddl	x9, x8, [x8]
    4bd0: f100051f     	cmp	x8, #0x1
    4bd4: 54000081     	b.ne	0x4be4 <_willow_panic_depth+0xfc>
    4bd8: d50339bf     	dmb	ishld
    4bdc: 910023e0     	add	x0, sp, #0x8
    4be0: 94000000     	bl	0x4be0 <_willow_panic_depth+0xf8>
    4be4: 12b00008     	mov	w8, #0x7fffffff         ; =2147483647
    4be8: eb08029f     	cmp	x20, x8
    4bec: 9a883288     	csel	x8, x20, x8, lo
    4bf0: 14000002     	b	0x4bf8 <_willow_panic_depth+0x110>
    4bf4: f9000009     	str	x9, [x0]
    4bf8: aa0803e0     	mov	x0, x8
    4bfc: a9437bfd     	ldp	x29, x30, [sp, #0x30]
    4c00: a9424ff4     	ldp	x20, x19, [sp, #0x20]
    4c04: a94157f6     	ldp	x22, x21, [sp, #0x10]
    4c08: 910103ff     	add	sp, sp, #0x40
    4c0c: d65f03c0     	ret
    4c10: 90000000     	adrp	x0, 0x4000 <__ZN4core3ptr94drop_in_place$LT$willow_runtime..stack_trace..replace_current..$u7b$$u7b$closure$u7d$$u7d$$GT$17h97a9ff60e65db6c5E.llvm.16627454700614441515+0x48>
    4c14: 91000000     	add	x0, x0, #0x0
    4c18: 94000000     	bl	0x4c18 <_willow_panic_depth+0x130>
    4c1c: d4200020     	brk	#0x1
    4c20: 91006260     	add	x0, x19, #0x18
    4c24: 94000000     	bl	0x4c24 <_willow_panic_depth+0x13c>
    4c28: 17ffffda     	b	0x4b90 <_willow_panic_depth+0xa8>
    4c2c: 94000000     	bl	0x4c2c <_willow_panic_depth+0x144>
    4c30: 3940827f     	ldrb	wzr, [x19, #0x20]
    4c34: f9401e74     	ldr	x20, [x19, #0x38]
    4c38: 35fffbc0     	cbnz	w0, 0x4bb0 <_willow_panic_depth+0xc8>
    4c3c: 17ffffe0     	b	0x4bbc <_willow_panic_depth+0xd4>
    4c40: 94000000     	bl	0x4c40 <_willow_panic_depth+0x158>
    4c44: 3707fbc0     	tbnz	w0, #0x0, 0x4bbc <_willow_panic_depth+0xd4>
    4c48: 52800028     	mov	w8, #0x1                ; =1
    4c4c: 39008268     	strb	w8, [x19, #0x20]
    4c50: 17ffffdb     	b	0x4bbc <_willow_panic_depth+0xd4>
    4c54: 94000000     	bl	0x4c54 <_willow_panic_depth+0x16c>
    4c58: f94007e8     	ldr	x8, [sp, #0x8]
    4c5c: 92800009     	mov	x9, #-0x1               ; =-1
    4c60: f8690108     	ldaddl	x9, x8, [x8]
    4c64: f100051f     	cmp	x8, #0x1
    4c68: 54000081     	b.ne	0x4c78 <_willow_panic_depth+0x190>
    4c6c: d50339bf     	dmb	ishld
    4c70: 910023e0     	add	x0, sp, #0x8
    4c74: 94000000     	bl	0x4c74 <_willow_panic_depth+0x18c>
    4c78: 94000000     	bl	0x4c78 <_willow_panic_depth+0x190>
    4c7c: 94000000     	bl	0x4c7c <_willow_panic_depth+0x194>
    4c80: 94000000     	bl	0x4c80 <_willow_panic_depth+0x198>
