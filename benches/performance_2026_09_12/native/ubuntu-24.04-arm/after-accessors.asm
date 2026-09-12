0000000000000000 <willow_root_depth>:
       0: a9bf7bfd     	stp	x29, x30, [sp, #-0x10]!
       4: 910003fd     	mov	x29, sp
       8: 90000000     	adrp	x0, 0x0 <willow_root_depth>
       c: f9400001     	ldr	x1, [x0]
      10: 91000000     	add	x0, x0, #0x0
      14: d63f0020     	blr	x1
      18: d53bd048     	mrs	x8, TPIDR_EL0
      1c: f8606900     	ldr	x0, [x8, x0]
      20: d35ffc08     	lsr	x8, x0, #31
      24: b5000068     	cbnz	x8, 0x30 <willow_root_depth+0x30>
      28: a8c17bfd     	ldp	x29, x30, [sp], #0x10
      2c: d65f03c0     	ret
      30: 90000000     	adrp	x0, 0x0 <willow_root_depth>
      34: 91000000     	add	x0, x0, #0x0
      38: 52800ca1     	mov	w1, #0x65               // =101
      3c: 94000000     	bl	0x3c <willow_root_depth+0x3c>
      40: 94000000     	bl	0x40 <willow_root_depth+0x40>
      44: d4200020     	brk	#0x1
      48: 94000000     	bl	0x48 <willow_root_depth+0x48>

0000000000000000 <willow_panic_depth>:
       0: a9be7bfd     	stp	x29, x30, [sp, #-0x20]!
       4: f9000bf3     	str	x19, [sp, #0x10]
       8: 910003fd     	mov	x29, sp
       c: 90000000     	adrp	x0, 0x0 <willow_panic_depth>
      10: f9400001     	ldr	x1, [x0]
      14: 91000000     	add	x0, x0, #0x0
      18: d63f0020     	blr	x1
      1c: d53bd048     	mrs	x8, TPIDR_EL0
      20: 8b000113     	add	x19, x8, x0
      24: 39404268     	ldrb	w8, [x19, #0x10]
      28: 7100051f     	cmp	w8, #0x1
      2c: 540001a0     	b.eq	0x60 <willow_panic_depth+0x60>
      30: 7100091f     	cmp	w8, #0x2
      34: 540000a1     	b.ne	0x48 <willow_panic_depth+0x48>
      38: 90000000     	adrp	x0, 0x0 <willow_panic_depth>
      3c: 91000000     	add	x0, x0, #0x0
      40: 94000000     	bl	0x40 <willow_panic_depth+0x40>
      44: 14000022     	b	0xcc <willow_panic_depth+0xcc>
      48: 90000001     	adrp	x1, 0x0 <willow_panic_depth>
      4c: 91000021     	add	x1, x1, #0x0
      50: aa1303e0     	mov	x0, x19
      54: 94000000     	bl	0x54 <willow_panic_depth+0x54>
      58: 52800028     	mov	w8, #0x1                // =1
      5c: 39004268     	strb	w8, [x19, #0x10]
      60: f9400268     	ldr	x8, [x19]
      64: 92f00009     	mov	x9, #0x7fffffffffffffff // =9223372036854775807
      68: eb09011f     	cmp	x8, x9
      6c: 540002a2     	b.hs	0xc0 <willow_panic_depth+0xc0>
      70: f9400669     	ldr	x9, [x19, #0x8]
      74: 9100050a     	add	x10, x8, #0x1
      78: f900026a     	str	x10, [x19]
      7c: b4000189     	cbz	x9, 0xac <willow_panic_depth+0xac>
      80: 91016128     	add	x8, x9, #0x58
      84: 12b00009     	mov	w9, #0x7fffffff         // =2147483647
      88: c8dffd08     	ldar	x8, [x8]
      8c: f940026a     	ldr	x10, [x19]
      90: eb09011f     	cmp	x8, x9
      94: 9a893100     	csel	x0, x8, x9, lo
      98: d1000548     	sub	x8, x10, #0x1
      9c: f9000268     	str	x8, [x19]
      a0: f9400bf3     	ldr	x19, [sp, #0x10]
      a4: a8c27bfd     	ldp	x29, x30, [sp], #0x20
      a8: d65f03c0     	ret
      ac: 2a1f03e0     	mov	w0, wzr
      b0: f9000268     	str	x8, [x19]
      b4: f9400bf3     	ldr	x19, [sp, #0x10]
      b8: a8c27bfd     	ldp	x29, x30, [sp], #0x20
      bc: d65f03c0     	ret
      c0: 90000000     	adrp	x0, 0x0 <willow_panic_depth>
      c4: 91000000     	add	x0, x0, #0x0
      c8: 94000000     	bl	0xc8 <willow_panic_depth+0xc8>
      cc: d4200020     	brk	#0x1
      d0: 94000000     	bl	0xd0 <willow_panic_depth+0xd0>
