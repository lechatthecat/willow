0000000000000000 <willow_root_depth>:
       0: 50                           	pushq	%rax
       1: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x8 <willow_root_depth+0x8>
       8: e8 00 00 00 00               	callq	0xd <willow_root_depth+0xd>
       d: 48 8b 80 00 00 00 00         	movq	(%rax), %rax
      14: 48 3d ff ff ff 7f            	cmpq	$0x7fffffff, %rax       # imm = 0x7FFFFFFF
      1a: 77 02                        	ja	0x1e <willow_root_depth+0x1e>
      1c: 59                           	popq	%rcx
      1d: c3                           	retq
      1e: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x25 <willow_root_depth+0x25>
      25: be 65 00 00 00               	movl	$0x65, %esi
      2a: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x30 <willow_root_depth+0x30>
      30: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x36 <willow_root_depth+0x36>
      36: 0f 0b                        	ud2
      38: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x3e <willow_root_depth+0x3e>

0000000000000000 <willow_panic_depth>:
       0: 53                           	pushq	%rbx
       1: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x8 <willow_panic_depth+0x8>
       8: e8 00 00 00 00               	callq	0xd <willow_panic_depth+0xd>
       d: 48 8d 98 00 00 00 00         	leaq	(%rax), %rbx
      14: 0f b6 80 00 00 00 00         	movzbl	(%rax), %eax
      1b: 83 f8 01                     	cmpl	$0x1, %eax
      1e: 74 28                        	je	0x48 <willow_panic_depth+0x48>
      20: 83 f8 02                     	cmpl	$0x2, %eax
      23: 75 0f                        	jne	0x34 <willow_panic_depth+0x34>
      25: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x2c <willow_panic_depth+0x2c>
      2c: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x32 <willow_panic_depth+0x32>
      32: eb 69                        	jmp	0x9d <willow_panic_depth+0x9d>
      34: 48 8d 35 00 00 00 00         	leaq	(%rip), %rsi            # 0x3b <willow_panic_depth+0x3b>
      3b: 48 89 df                     	movq	%rbx, %rdi
      3e: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x44 <willow_panic_depth+0x44>
      44: c6 43 10 01                  	movb	$0x1, 0x10(%rbx)
      48: 48 8b 0b                     	movq	(%rbx), %rcx
      4b: 48 b8 ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %rax # imm = 0x7FFFFFFFFFFFFFFF
      55: 48 39 c1                     	cmpq	%rax, %rcx
      58: 73 36                        	jae	0x90 <willow_panic_depth+0x90>
      5a: 48 8d 41 01                  	leaq	0x1(%rcx), %rax
      5e: 48 89 03                     	movq	%rax, (%rbx)
      61: 48 8b 43 08                  	movq	0x8(%rbx), %rax
      65: 48 85 c0                     	testq	%rax, %rax
      68: 74 1f                        	je	0x89 <willow_panic_depth+0x89>
      6a: 48 8b 48 58                  	movq	0x58(%rax), %rcx
      6e: 48 81 f9 ff ff ff 7f         	cmpq	$0x7fffffff, %rcx       # imm = 0x7FFFFFFF
      75: b8 ff ff ff 7f               	movl	$0x7fffffff, %eax       # imm = 0x7FFFFFFF
      7a: 48 0f 42 c1                  	cmovbq	%rcx, %rax
      7e: 48 8b 0b                     	movq	(%rbx), %rcx
      81: 48 ff c9                     	decq	%rcx
      84: 48 89 0b                     	movq	%rcx, (%rbx)
      87: 5b                           	popq	%rbx
      88: c3                           	retq
      89: 31 c0                        	xorl	%eax, %eax
      8b: 48 89 0b                     	movq	%rcx, (%rbx)
      8e: 5b                           	popq	%rbx
      8f: c3                           	retq
      90: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x97 <willow_panic_depth+0x97>
      97: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x9d <willow_panic_depth+0x9d>
      9d: 0f 0b                        	ud2
      9f: ff 15 00 00 00 00            	callq	*(%rip)                 # 0xa5 <willow_panic_depth+0xa5>
