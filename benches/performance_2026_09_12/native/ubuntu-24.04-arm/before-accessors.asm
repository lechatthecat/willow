0000000000000000 <willow_root_depth>:
       0: a9be7bfd     	stp	x29, x30, [sp, #-0x20]!
       4: f9000bf3     	str	x19, [sp, #0x10]
       8: 910003fd     	mov	x29, sp
       c: 90000000     	adrp	x0, 0x0 <willow_root_depth>
      10: f9400001     	ldr	x1, [x0]
      14: 91000000     	add	x0, x0, #0x0
      18: d63f0020     	blr	x1
      1c: d53bd048     	mrs	x8, TPIDR_EL0
      20: 8b000113     	add	x19, x8, x0
      24: 39408268     	ldrb	w8, [x19, #0x20]
      28: 7100051f     	cmp	w8, #0x1
      2c: 540001a0     	b.eq	0x60 <willow_root_depth+0x60>
      30: 7100091f     	cmp	w8, #0x2
      34: 540000a1     	b.ne	0x48 <willow_root_depth+0x48>
      38: 90000000     	adrp	x0, 0x0 <willow_root_depth>
      3c: 91000000     	add	x0, x0, #0x0
      40: 94000000     	bl	0x40 <willow_root_depth+0x40>
      44: 1400001d     	b	0xb8 <willow_root_depth+0xb8>
      48: 90000001     	adrp	x1, 0x0 <willow_root_depth>
      4c: 91000021     	add	x1, x1, #0x0
      50: aa1303e0     	mov	x0, x19
      54: 94000000     	bl	0x54 <willow_root_depth+0x54>
      58: 52800028     	mov	w8, #0x1                // =1
      5c: 39008268     	strb	w8, [x19, #0x20]
      60: f9400268     	ldr	x8, [x19]
      64: 92f00009     	mov	x9, #0x7fffffffffffffff // =9223372036854775807
      68: eb09011f     	cmp	x8, x9
      6c: 54000142     	b.hs	0x94 <willow_root_depth+0x94>
      70: f9400e60     	ldr	x0, [x19, #0x18]
      74: 9100050a     	add	x10, x8, #0x1
      78: f900026a     	str	x10, [x19]
      7c: d35ffc09     	lsr	x9, x0, #31
      80: b5000129     	cbnz	x9, 0xa4 <willow_root_depth+0xa4>
      84: f9000268     	str	x8, [x19]
      88: f9400bf3     	ldr	x19, [sp, #0x10]
      8c: a8c27bfd     	ldp	x29, x30, [sp], #0x20
      90: d65f03c0     	ret
      94: 90000000     	adrp	x0, 0x0 <willow_root_depth>
      98: 91000000     	add	x0, x0, #0x0
      9c: 94000000     	bl	0x9c <willow_root_depth+0x9c>
      a0: 14000006     	b	0xb8 <willow_root_depth+0xb8>
      a4: 90000000     	adrp	x0, 0x0 <willow_root_depth>
      a8: 91000000     	add	x0, x0, #0x0
      ac: 52800ca1     	mov	w1, #0x65               // =101
      b0: 94000000     	bl	0xb0 <willow_root_depth+0xb0>
      b4: 94000000     	bl	0xb4 <willow_root_depth+0xb4>
      b8: d4200020     	brk	#0x1
      bc: f9400268     	ldr	x8, [x19]
      c0: d1000508     	sub	x8, x8, #0x1
      c4: f9000268     	str	x8, [x19]
      c8: 94000000     	bl	0xc8 <willow_root_depth+0xc8>
      cc: 94000000     	bl	0xcc <willow_root_depth+0xcc>

0000000000000000 <willow_panic_depth>:
       0: a9bd7bfd     	stp	x29, x30, [sp, #-0x30]!
       4: f9000bf5     	str	x21, [sp, #0x10]
       8: a9024ff4     	stp	x20, x19, [sp, #0x20]
       c: 910003fd     	mov	x29, sp
      10: 90000000     	adrp	x0, 0x0 <willow_panic_depth>
      14: f9400001     	ldr	x1, [x0]
      18: 91000000     	add	x0, x0, #0x0
      1c: d63f0020     	blr	x1
      20: d53bd048     	mrs	x8, TPIDR_EL0
      24: 8b000113     	add	x19, x8, x0
      28: 39404268     	ldrb	w8, [x19, #0x10]
      2c: 7100051f     	cmp	w8, #0x1
      30: 540001a0     	b.eq	0x64 <willow_panic_depth+0x64>
      34: 7100091f     	cmp	w8, #0x2
      38: 540000a1     	b.ne	0x4c <willow_panic_depth+0x4c>
      3c: 90000000     	adrp	x0, 0x0 <willow_panic_depth>
      40: 91000000     	add	x0, x0, #0x0
      44: 94000000     	bl	0x44 <willow_panic_depth+0x44>
      48: 14000043     	b	0x154 <willow_panic_depth+0x154>
      4c: 90000001     	adrp	x1, 0x0 <willow_panic_depth>
      50: 91000021     	add	x1, x1, #0x0
      54: aa1303e0     	mov	x0, x19
      58: 94000000     	bl	0x58 <willow_panic_depth+0x58>
      5c: 52800028     	mov	w8, #0x1                // =1
      60: 39004268     	strb	w8, [x19, #0x10]
      64: f9400268     	ldr	x8, [x19]
      68: 92f00009     	mov	x9, #0x7fffffffffffffff // =9223372036854775807
      6c: eb09011f     	cmp	x8, x9
      70: 540006c2     	b.hs	0x148 <willow_panic_depth+0x148>
      74: f9400661     	ldr	x1, [x19, #0x8]
      78: 91000509     	add	x9, x8, #0x1
      7c: f9000269     	str	x9, [x19]
      80: b4000581     	cbz	x1, 0x130 <willow_panic_depth+0x130>
      84: 52800020     	mov	w0, #0x1                // =1
      88: 94000000     	bl	0x88 <willow_panic_depth+0x88>
      8c: b7f80640     	tbnz	x0, #0x3f, 0x154 <willow_panic_depth+0x154>
      90: a9405268     	ldp	x8, x20, [x19]
      94: 2a1f03e0     	mov	w0, wzr
      98: 52800021     	mov	w1, #0x1                // =1
      9c: d1000508     	sub	x8, x8, #0x1
      a0: 91004282     	add	x2, x20, #0x10
      a4: f9000fb4     	str	x20, [x29, #0x18]
      a8: f9000268     	str	x8, [x19]
      ac: 94000000     	bl	0xac <willow_panic_depth+0xac>
      b0: 7100001f     	cmp	w0, #0x0
      b4: 54000521     	b.ne	0x158 <willow_panic_depth+0x158>
      b8: 90000015     	adrp	x21, 0x0 <willow_panic_depth>
      bc: f94002b5     	ldr	x21, [x21]
      c0: f94002a8     	ldr	x8, [x21]
      c4: f240f91f     	tst	x8, #0x7fffffffffffffff
      c8: 540004e1     	b.ne	0x164 <willow_panic_depth+0x164>
      cc: 3940529f     	ldrb	wzr, [x20, #0x14]
      d0: f9401693     	ldr	x19, [x20, #0x28]
      d4: f94002a8     	ldr	x8, [x21]
      d8: f240f91f     	tst	x8, #0x7fffffffffffffff
      dc: 54000541     	b.ne	0x184 <willow_panic_depth+0x184>
      e0: 91004281     	add	x1, x20, #0x10
      e4: 2a1f03e0     	mov	w0, wzr
      e8: 94000000     	bl	0xe8 <willow_panic_depth+0xe8>
      ec: 7100081f     	cmp	w0, #0x2
      f0: 54000440     	b.eq	0x178 <willow_panic_depth+0x178>
      f4: f9400fa1     	ldr	x1, [x29, #0x18]
      f8: 92800000     	mov	x0, #-0x1               // =-1
      fc: 94000000     	bl	0xfc <willow_panic_depth+0xfc>
     100: f100041f     	cmp	x0, #0x1
     104: 54000081     	b.ne	0x114 <willow_panic_depth+0x114>
     108: d50339bf     	dmb	ishld
     10c: 910063a0     	add	x0, x29, #0x18
     110: 94000000     	bl	0x110 <willow_panic_depth+0x110>
     114: 12b00008     	mov	w8, #0x7fffffff         // =2147483647
     118: eb08027f     	cmp	x19, x8
     11c: 9a883260     	csel	x0, x19, x8, lo
     120: a9424ff4     	ldp	x20, x19, [sp, #0x20]
     124: f9400bf5     	ldr	x21, [sp, #0x10]
     128: a8c37bfd     	ldp	x29, x30, [sp], #0x30
     12c: d65f03c0     	ret
     130: 2a1f03e0     	mov	w0, wzr
     134: f9000268     	str	x8, [x19]
     138: a9424ff4     	ldp	x20, x19, [sp, #0x20]
     13c: f9400bf5     	ldr	x21, [sp, #0x10]
     140: a8c37bfd     	ldp	x29, x30, [sp], #0x30
     144: d65f03c0     	ret
     148: 90000000     	adrp	x0, 0x0 <willow_panic_depth>
     14c: 91000000     	add	x0, x0, #0x0
     150: 94000000     	bl	0x150 <willow_panic_depth+0x150>
     154: d4200020     	brk	#0x1
     158: 91004280     	add	x0, x20, #0x10
     15c: 94000000     	bl	0x15c <willow_panic_depth+0x15c>
     160: 17ffffd6     	b	0xb8 <willow_panic_depth+0xb8>
     164: 94000000     	bl	0x164 <willow_panic_depth+0x164>
     168: 3940529f     	ldrb	wzr, [x20, #0x14]
     16c: f9401693     	ldr	x19, [x20, #0x28]
     170: 35fffb20     	cbnz	w0, 0xd4 <willow_panic_depth+0xd4>
     174: 17ffffdb     	b	0xe0 <willow_panic_depth+0xe0>
     178: 91004280     	add	x0, x20, #0x10
     17c: 94000000     	bl	0x17c <willow_panic_depth+0x17c>
     180: 17ffffdd     	b	0xf4 <willow_panic_depth+0xf4>
     184: 94000000     	bl	0x184 <willow_panic_depth+0x184>
     188: 3707fac0     	tbnz	w0, #0x0, 0xe0 <willow_panic_depth+0xe0>
     18c: 52800028     	mov	w8, #0x1                // =1
     190: 39005288     	strb	w8, [x20, #0x14]
     194: 17ffffd3     	b	0xe0 <willow_panic_depth+0xe0>
     198: f9400fa1     	ldr	x1, [x29, #0x18]
     19c: 92800000     	mov	x0, #-0x1               // =-1
     1a0: 94000000     	bl	0x1a0 <willow_panic_depth+0x1a0>
     1a4: f100041f     	cmp	x0, #0x1
     1a8: 540000c1     	b.ne	0x1c0 <willow_panic_depth+0x1c0>
     1ac: d50339bf     	dmb	ishld
     1b0: 910063a0     	add	x0, x29, #0x18
     1b4: 94000000     	bl	0x1b4 <willow_panic_depth+0x1b4>
     1b8: 14000002     	b	0x1c0 <willow_panic_depth+0x1c0>
     1bc: 94000000     	bl	0x1bc <willow_panic_depth+0x1bc>
     1c0: 94000000     	bl	0x1c0 <willow_panic_depth+0x1c0>
     1c4: 94000000     	bl	0x1c4 <willow_panic_depth+0x1c4>
