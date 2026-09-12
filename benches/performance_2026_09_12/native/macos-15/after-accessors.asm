0000000000016208 <_willow_root_depth>:
   16208: a9bf7bfd     	stp	x29, x30, [sp, #-0x10]!
   1620c: 910003fd     	mov	x29, sp
   16210: 90000000     	adrp	x0, 0x16000 <_willow_pop_roots+0x10>
   16214: f9400000     	ldr	x0, [x0]
   16218: f9400008     	ldr	x8, [x0]
   1621c: d63f0100     	blr	x8
   16220: f9400000     	ldr	x0, [x0]
   16224: d35ffc08     	lsr	x8, x0, #31
   16228: b5000068     	cbnz	x8, 0x16234 <_willow_root_depth+0x2c>
   1622c: a8c17bfd     	ldp	x29, x30, [sp], #0x10
   16230: d65f03c0     	ret
   16234: 90000000     	adrp	x0, 0x16000 <_willow_pop_roots+0x10>
   16238: 91000000     	add	x0, x0, #0x0
   1623c: 52800ca1     	mov	w1, #0x65               ; =101
   16240: 94000000     	bl	0x16240 <_willow_root_depth+0x38>
   16244: 94000000     	bl	0x16244 <_willow_root_depth+0x3c>
   16248: d4200020     	brk	#0x1
   1624c: 94000000     	bl	0x1624c <_willow_root_depth+0x44>

0000000000008f44 <_willow_panic_depth>:
    8f44: a9be4ff4     	stp	x20, x19, [sp, #-0x20]!
    8f48: a9017bfd     	stp	x29, x30, [sp, #0x10]
    8f4c: 910043fd     	add	x29, sp, #0x10
    8f50: 90000000     	adrp	x0, 0x8000 <_willow_map_get+0x174>
    8f54: f9400000     	ldr	x0, [x0]
    8f58: f9400008     	ldr	x8, [x0]
    8f5c: d63f0100     	blr	x8
    8f60: 39404008     	ldrb	w8, [x0, #0x10]
    8f64: 7100051f     	cmp	w8, #0x1
    8f68: 540001c0     	b.eq	0x8fa0 <_willow_panic_depth+0x5c>
    8f6c: 7100091f     	cmp	w8, #0x2
    8f70: 540000a1     	b.ne	0x8f84 <_willow_panic_depth+0x40>
    8f74: 90000000     	adrp	x0, 0x8000 <_willow_map_get+0x174>
    8f78: 91000000     	add	x0, x0, #0x0
    8f7c: 94000000     	bl	0x8f7c <_willow_panic_depth+0x38>
    8f80: 1400001e     	b	0x8ff8 <_willow_panic_depth+0xb4>
    8f84: 90000001     	adrp	x1, 0x8000 <_willow_map_get+0x174>
    8f88: 91000021     	add	x1, x1, #0x0
    8f8c: aa0003f3     	mov	x19, x0
    8f90: 94000000     	bl	0x8f90 <_willow_panic_depth+0x4c>
    8f94: 52800028     	mov	w8, #0x1                ; =1
    8f98: aa1303e0     	mov	x0, x19
    8f9c: 39004268     	strb	w8, [x19, #0x10]
    8fa0: f9400009     	ldr	x9, [x0]
    8fa4: 92f00008     	mov	x8, #0x7fffffffffffffff ; =9223372036854775807
    8fa8: eb08013f     	cmp	x9, x8
    8fac: 54000202     	b.hs	0x8fec <_willow_panic_depth+0xa8>
    8fb0: 91000528     	add	x8, x9, #0x1
    8fb4: f9000008     	str	x8, [x0]
    8fb8: f9400408     	ldr	x8, [x0, #0x8]
    8fbc: b40000e8     	cbz	x8, 0x8fd8 <_willow_panic_depth+0x94>
    8fc0: d9460108     	ldapur	x8, [x8, #0x60]
    8fc4: 12b00009     	mov	w9, #0x7fffffff         ; =2147483647
    8fc8: eb09011f     	cmp	x8, x9
    8fcc: 9a893108     	csel	x8, x8, x9, lo
    8fd0: f9400009     	ldr	x9, [x0]
    8fd4: d1000529     	sub	x9, x9, #0x1
    8fd8: f9000009     	str	x9, [x0]
    8fdc: aa0803e0     	mov	x0, x8
    8fe0: a9417bfd     	ldp	x29, x30, [sp, #0x10]
    8fe4: a8c24ff4     	ldp	x20, x19, [sp], #0x20
    8fe8: d65f03c0     	ret
    8fec: 90000000     	adrp	x0, 0x8000 <_willow_map_get+0x174>
    8ff0: 91000000     	add	x0, x0, #0x0
    8ff4: 94000000     	bl	0x8ff4 <_willow_panic_depth+0xb0>
    8ff8: d4200020     	brk	#0x1
    8ffc: 94000000     	bl	0x8ffc <_willow_panic_depth+0xb8>
