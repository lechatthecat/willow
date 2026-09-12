0000000000000000 <willow_root_depth>:
       0: 53                           	pushq	%rbx
       1: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x8 <willow_root_depth+0x8>
       8: e8 00 00 00 00               	callq	0xd <willow_root_depth+0xd>
       d: 48 8d 98 00 00 00 00         	leaq	(%rax), %rbx
      14: 0f b6 80 00 00 00 00         	movzbl	(%rax), %eax
      1b: 83 f8 01                     	cmpl	$0x1, %eax
      1e: 74 28                        	je	0x48 <willow_root_depth+0x48>
      20: 83 f8 02                     	cmpl	$0x2, %eax
      23: 75 0f                        	jne	0x34 <willow_root_depth+0x34>
      25: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x2c <willow_root_depth+0x2c>
      2c: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x32 <willow_root_depth+0x32>
      32: eb 65                        	jmp	0x99 <willow_root_depth+0x99>
      34: 48 8d 35 00 00 00 00         	leaq	(%rip), %rsi            # 0x3b <willow_root_depth+0x3b>
      3b: 48 89 df                     	movq	%rbx, %rdi
      3e: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x44 <willow_root_depth+0x44>
      44: c6 43 20 01                  	movb	$0x1, 0x20(%rbx)
      48: 48 8b 0b                     	movq	(%rbx), %rcx
      4b: 48 b8 ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %rax # imm = 0x7FFFFFFFFFFFFFFF
      55: 48 39 c1                     	cmpq	%rax, %rcx
      58: 73 18                        	jae	0x72 <willow_root_depth+0x72>
      5a: 48 8d 41 01                  	leaq	0x1(%rcx), %rax
      5e: 48 89 03                     	movq	%rax, (%rbx)
      61: 48 8b 43 18                  	movq	0x18(%rbx), %rax
      65: 48 a9 00 00 00 80            	testq	$-0x80000000, %rax      # imm = 0x80000000
      6b: 75 14                        	jne	0x81 <willow_root_depth+0x81>
      6d: 48 89 0b                     	movq	%rcx, (%rbx)
      70: 5b                           	popq	%rbx
      71: c3                           	retq
      72: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x79 <willow_root_depth+0x79>
      79: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x7f <willow_root_depth+0x7f>
      7f: eb 18                        	jmp	0x99 <willow_root_depth+0x99>
      81: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x88 <willow_root_depth+0x88>
      88: be 65 00 00 00               	movl	$0x65, %esi
      8d: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x93 <willow_root_depth+0x93>
      93: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x99 <willow_root_depth+0x99>
      99: 0f 0b                        	ud2
      9b: 48 ff 0b                     	decq	(%rbx)
      9e: ff 15 00 00 00 00            	callq	*(%rip)                 # 0xa4 <willow_root_depth+0xa4>
      a4: ff 15 00 00 00 00            	callq	*(%rip)                 # 0xaa <willow_root_depth+0xaa>

0000000000000000 <willow_panic_depth>:
       0: 41 57                        	pushq	%r15
       2: 41 56                        	pushq	%r14
       4: 41 55                        	pushq	%r13
       6: 41 54                        	pushq	%r12
       8: 53                           	pushq	%rbx
       9: 48 83 ec 10                  	subq	$0x10, %rsp
       d: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x14 <willow_panic_depth+0x14>
      14: e8 00 00 00 00               	callq	0x19 <willow_panic_depth+0x19>
      19: 48 8d 98 00 00 00 00         	leaq	(%rax), %rbx
      20: 0f b6 80 00 00 00 00         	movzbl	(%rax), %eax
      27: 83 f8 01                     	cmpl	$0x1, %eax
      2a: 74 2b                        	je	0x57 <willow_panic_depth+0x57>
      2c: 83 f8 02                     	cmpl	$0x2, %eax
      2f: 75 12                        	jne	0x43 <willow_panic_depth+0x43>
      31: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x38 <willow_panic_depth+0x38>
      38: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x3e <willow_panic_depth+0x3e>
      3e: e9 e2 00 00 00               	jmp	0x125 <willow_panic_depth+0x125>
      43: 48 8d 35 00 00 00 00         	leaq	(%rip), %rsi            # 0x4a <willow_panic_depth+0x4a>
      4a: 48 89 df                     	movq	%rbx, %rdi
      4d: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x53 <willow_panic_depth+0x53>
      53: c6 43 10 01                  	movb	$0x1, 0x10(%rbx)
      57: 49 bf ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %r15 # imm = 0x7FFFFFFFFFFFFFFF
      61: 48 8b 03                     	movq	(%rbx), %rax
      64: 4c 39 f8                     	cmpq	%r15, %rax
      67: 0f 83 ab 00 00 00            	jae	0x118 <willow_panic_depth+0x118>
      6d: 48 8d 48 01                  	leaq	0x1(%rax), %rcx
      71: 48 89 0b                     	movq	%rcx, (%rbx)
      74: 48 8b 4b 08                  	movq	0x8(%rbx), %rcx
      78: 48 85 c9                     	testq	%rcx, %rcx
      7b: 0f 84 84 00 00 00            	je	0x105 <willow_panic_depth+0x105>
      81: f0                           	lock
      82: 48 ff 01                     	incq	(%rcx)
      85: 0f 8e 9a 00 00 00            	jle	0x125 <willow_panic_depth+0x125>
      8b: 4c 8b 63 08                  	movq	0x8(%rbx), %r12
      8f: 48 ff 0b                     	decq	(%rbx)
      92: 4c 89 64 24 08               	movq	%r12, 0x8(%rsp)
      97: 49 8d 5c 24 10               	leaq	0x10(%r12), %rbx
      9c: b9 01 00 00 00               	movl	$0x1, %ecx
      a1: 31 c0                        	xorl	%eax, %eax
      a3: f0                           	lock
      a4: 41 0f b1 4c 24 10            	cmpxchgl	%ecx, 0x10(%r12)
      aa: 75 7b                        	jne	0x127 <willow_panic_depth+0x127>
      ac: 4c 8b 2d 00 00 00 00         	movq	(%rip), %r13            # 0xb3 <willow_panic_depth+0xb3>
      b3: 49 8b 45 00                  	movq	(%r13), %rax
      b7: 4c 85 f8                     	testq	%r15, %rax
      ba: 75 79                        	jne	0x135 <willow_panic_depth+0x135>
      bc: 41 0f b6 44 24 14            	movzbl	0x14(%r12), %eax
      c2: 4d 8b 74 24 28               	movq	0x28(%r12), %r14
      c7: 49 8b 45 00                  	movq	(%r13), %rax
      cb: 4c 85 f8                     	testq	%r15, %rax
      ce: 0f 85 87 00 00 00            	jne	0x15b <willow_panic_depth+0x15b>
      d4: 31 c0                        	xorl	%eax, %eax
      d6: 87 03                        	xchgl	%eax, (%rbx)
      d8: 83 f8 02                     	cmpl	$0x2, %eax
      db: 74 73                        	je	0x150 <willow_panic_depth+0x150>
      dd: 48 8b 44 24 08               	movq	0x8(%rsp), %rax
      e2: f0                           	lock
      e3: 48 ff 08                     	decq	(%rax)
      e6: 75 0b                        	jne	0xf3 <willow_panic_depth+0xf3>
      e8: 48 8d 7c 24 08               	leaq	0x8(%rsp), %rdi
      ed: ff 15 00 00 00 00            	callq	*(%rip)                 # 0xf3 <willow_panic_depth+0xf3>
      f3: 49 81 fe ff ff ff 7f         	cmpq	$0x7fffffff, %r14       # imm = 0x7FFFFFFF
      fa: b8 ff ff ff 7f               	movl	$0x7fffffff, %eax       # imm = 0x7FFFFFFF
      ff: 49 0f 42 c6                  	cmovbq	%r14, %rax
     103: eb 05                        	jmp	0x10a <willow_panic_depth+0x10a>
     105: 48 89 03                     	movq	%rax, (%rbx)
     108: 31 c0                        	xorl	%eax, %eax
     10a: 48 83 c4 10                  	addq	$0x10, %rsp
     10e: 5b                           	popq	%rbx
     10f: 41 5c                        	popq	%r12
     111: 41 5d                        	popq	%r13
     113: 41 5e                        	popq	%r14
     115: 41 5f                        	popq	%r15
     117: c3                           	retq
     118: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            # 0x11f <willow_panic_depth+0x11f>
     11f: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x125 <willow_panic_depth+0x125>
     125: 0f 0b                        	ud2
     127: 48 89 df                     	movq	%rbx, %rdi
     12a: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x130 <willow_panic_depth+0x130>
     130: e9 77 ff ff ff               	jmp	0xac <willow_panic_depth+0xac>
     135: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x13b <willow_panic_depth+0x13b>
     13b: 41 0f b6 4c 24 14            	movzbl	0x14(%r12), %ecx
     141: 4d 8b 74 24 28               	movq	0x28(%r12), %r14
     146: 84 c0                        	testb	%al, %al
     148: 0f 85 79 ff ff ff            	jne	0xc7 <willow_panic_depth+0xc7>
     14e: eb 84                        	jmp	0xd4 <willow_panic_depth+0xd4>
     150: 48 89 df                     	movq	%rbx, %rdi
     153: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x159 <willow_panic_depth+0x159>
     159: eb 82                        	jmp	0xdd <willow_panic_depth+0xdd>
     15b: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x161 <willow_panic_depth+0x161>
     161: 84 c0                        	testb	%al, %al
     163: 0f 85 6b ff ff ff            	jne	0xd4 <willow_panic_depth+0xd4>
     169: 41 c6 44 24 14 01            	movb	$0x1, 0x14(%r12)
     16f: e9 60 ff ff ff               	jmp	0xd4 <willow_panic_depth+0xd4>
     174: 48 8b 44 24 08               	movq	0x8(%rsp), %rax
     179: f0                           	lock
     17a: 48 ff 08                     	decq	(%rax)
     17d: 75 13                        	jne	0x192 <willow_panic_depth+0x192>
     17f: 48 8d 7c 24 08               	leaq	0x8(%rsp), %rdi
     184: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x18a <willow_panic_depth+0x18a>
     18a: eb 06                        	jmp	0x192 <willow_panic_depth+0x192>
     18c: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x192 <willow_panic_depth+0x192>
     192: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x198 <willow_panic_depth+0x198>
     198: ff 15 00 00 00 00            	callq	*(%rip)                 # 0x19e <willow_panic_depth+0x19e>
