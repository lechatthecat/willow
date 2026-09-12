0000000000000000 <willow_root_depth>:
       0: 55                           	pushq	%rbp
       1: 48 83 ec 30                  	subq	$0x30, %rsp
       5: 48 8d 6c 24 30               	leaq	0x30(%rsp), %rbp
       a: 48 c7 45 f8 fe ff ff ff      	movq	$-0x2, -0x8(%rbp)
      12: 8b 05 00 00 00 00            	movl	(%rip), %eax            # 0x18 <willow_root_depth+0x18>
      18: 65 48 8b 0c 25 58 00 00 00   	movq	%gs:0x58, %rcx
      21: 48 8b 04 c1                  	movq	(%rcx,%rax,8), %rax
      25: 48 8b 80 00 00 00 00         	movq	(%rax), %rax
      2c: 48 3d ff ff ff 7f            	cmpq	$0x7fffffff, %rax       # imm = 0x7FFFFFFF
      32: 77 06                        	ja	0x3a <willow_root_depth+0x3a>
      34: 48 83 c4 30                  	addq	$0x30, %rsp
      38: 5d                           	popq	%rbp
      39: c3                           	retq
      3a: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0x41 <willow_root_depth+0x41>
      41: ba 65 00 00 00               	movl	$0x65, %edx
      46: e8 00 00 00 00               	callq	0x4b <willow_root_depth+0x4b>
      4b: 90                           	nop
      4c: b9 07 00 00 00               	movl	$0x7, %ecx
      51: cd 29                        	int	$0x29
      53: 0f 0b                        	ud2
      55: 66 66 2e 0f 1f 84 00 00 00 00 00     	nopw	%cs:(%rax,%rax)

0000000000000000 <willow_panic_depth>:
       0: 55                           	pushq	%rbp
       1: 56                           	pushq	%rsi
       2: 48 83 ec 28                  	subq	$0x28, %rsp
       6: 48 8d 6c 24 20               	leaq	0x20(%rsp), %rbp
       b: 48 c7 45 00 fe ff ff ff      	movq	$-0x2, (%rbp)
      13: 8b 05 00 00 00 00            	movl	(%rip), %eax            # 0x19 <willow_panic_depth+0x19>
      19: 65 48 8b 0c 25 58 00 00 00   	movq	%gs:0x58, %rcx
      22: 48 8b 04 c1                  	movq	(%rcx,%rax,8), %rax
      26: 48 8d b0 00 00 00 00         	leaq	(%rax), %rsi
      2d: 0f b6 80 10 00 00 00         	movzbl	0x10(%rax), %eax
      34: 83 f8 01                     	cmpl	$0x1, %eax
      37: 74 28                        	je	0x61 <willow_panic_depth+0x61>
      39: 83 f8 02                     	cmpl	$0x2, %eax
      3c: 75 0f                        	jne	0x4d <willow_panic_depth+0x4d>
      3e: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0x45 <willow_panic_depth+0x45>
      45: e8 00 00 00 00               	callq	0x4a <willow_panic_depth+0x4a>
      4a: 90                           	nop
      4b: eb 6b                        	jmp	0xb8 <willow_panic_depth+0xb8>
      4d: 48 8d 15 00 00 00 00         	leaq	(%rip), %rdx            # 0x54 <willow_panic_depth+0x54>
      54: 48 89 f1                     	movq	%rsi, %rcx
      57: e8 00 00 00 00               	callq	0x5c <willow_panic_depth+0x5c>
      5c: 90                           	nop
      5d: c6 46 10 01                  	movb	$0x1, 0x10(%rsi)
      61: 48 8b 0e                     	movq	(%rsi), %rcx
      64: 48 b8 ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %rax # imm = 0x7FFFFFFFFFFFFFFF
      6e: 48 39 c1                     	cmpq	%rax, %rcx
      71: 73 38                        	jae	0xab <willow_panic_depth+0xab>
      73: 48 8d 41 01                  	leaq	0x1(%rcx), %rax
      77: 48 89 06                     	movq	%rax, (%rsi)
      7a: 48 8b 46 08                  	movq	0x8(%rsi), %rax
      7e: 48 85 c0                     	testq	%rax, %rax
      81: 74 1c                        	je	0x9f <willow_panic_depth+0x9f>
      83: 48 8b 48 58                  	movq	0x58(%rax), %rcx
      87: 48 81 f9 ff ff ff 7f         	cmpq	$0x7fffffff, %rcx       # imm = 0x7FFFFFFF
      8e: b8 ff ff ff 7f               	movl	$0x7fffffff, %eax       # imm = 0x7FFFFFFF
      93: 48 0f 42 c1                  	cmovbq	%rcx, %rax
      97: 48 8b 0e                     	movq	(%rsi), %rcx
      9a: 48 ff c9                     	decq	%rcx
      9d: eb 02                        	jmp	0xa1 <willow_panic_depth+0xa1>
      9f: 31 c0                        	xorl	%eax, %eax
      a1: 48 89 0e                     	movq	%rcx, (%rsi)
      a4: 48 83 c4 28                  	addq	$0x28, %rsp
      a8: 5e                           	popq	%rsi
      a9: 5d                           	popq	%rbp
      aa: c3                           	retq
      ab: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0xb2 <willow_panic_depth+0xb2>
      b2: e8 00 00 00 00               	callq	0xb7 <willow_panic_depth+0xb7>
      b7: 90                           	nop
      b8: 0f 0b                        	ud2
      ba: 66 0f 1f 44 00 00            	nopw	(%rax,%rax)
