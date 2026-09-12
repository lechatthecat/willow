0000000000000000 <willow_root_depth>:
       0: 55                           	pushq	%rbp
       1: 56                           	pushq	%rsi
       2: 48 83 ec 38                  	subq	$0x38, %rsp
       6: 48 8d 6c 24 30               	leaq	0x30(%rsp), %rbp
       b: 48 c7 45 00 fe ff ff ff      	movq	$-0x2, (%rbp)
      13: 8b 05 00 00 00 00            	movl	(%rip), %eax            # 0x19 <willow_root_depth+0x19>
      19: 65 48 8b 0c 25 58 00 00 00   	movq	%gs:0x58, %rcx
      22: 48 8b 04 c1                  	movq	(%rcx,%rax,8), %rax
      26: 48 8d b0 00 00 00 00         	leaq	(%rax), %rsi
      2d: 0f b6 80 20 00 00 00         	movzbl	0x20(%rax), %eax
      34: 83 f8 01                     	cmpl	$0x1, %eax
      37: 74 28                        	je	0x61 <willow_root_depth+0x61>
      39: 83 f8 02                     	cmpl	$0x2, %eax
      3c: 75 0f                        	jne	0x4d <willow_root_depth+0x4d>
      3e: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0x45 <willow_root_depth+0x45>
      45: e8 00 00 00 00               	callq	0x4a <willow_root_depth+0x4a>
      4a: 90                           	nop
      4b: eb 50                        	jmp	0x9d <willow_root_depth+0x9d>
      4d: 48 8d 15 00 00 00 00         	leaq	(%rip), %rdx            # 0x54 <willow_root_depth+0x54>
      54: 48 89 f1                     	movq	%rsi, %rcx
      57: e8 00 00 00 00               	callq	0x5c <willow_root_depth+0x5c>
      5c: 90                           	nop
      5d: c6 46 20 01                  	movb	$0x1, 0x20(%rsi)
      61: 48 8b 0e                     	movq	(%rsi), %rcx
      64: 48 b8 ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %rax # imm = 0x7FFFFFFFFFFFFFFF
      6e: 48 39 c1                     	cmpq	%rax, %rcx
      71: 73 1d                        	jae	0x90 <willow_root_depth+0x90>
      73: 48 8d 41 01                  	leaq	0x1(%rcx), %rax
      77: 48 89 06                     	movq	%rax, (%rsi)
      7a: 48 8b 46 18                  	movq	0x18(%rsi), %rax
      7e: 48 a9 00 00 00 80            	testq	$-0x80000000, %rax      # imm = 0x80000000
      84: 75 19                        	jne	0x9f <willow_root_depth+0x9f>
      86: 48 89 0e                     	movq	%rcx, (%rsi)
      89: 48 83 c4 38                  	addq	$0x38, %rsp
      8d: 5e                           	popq	%rsi
      8e: 5d                           	popq	%rbp
      8f: c3                           	retq
      90: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0x97 <willow_root_depth+0x97>
      97: e8 00 00 00 00               	callq	0x9c <willow_root_depth+0x9c>
      9c: 90                           	nop
      9d: 0f 0b                        	ud2
      9f: 48 89 75 f8                  	movq	%rsi, -0x8(%rbp)
      a3: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0xaa <willow_root_depth+0xaa>
      aa: ba 65 00 00 00               	movl	$0x65, %edx
      af: e8 00 00 00 00               	callq	0xb4 <willow_root_depth+0xb4>
      b4: 90                           	nop
      b5: b9 07 00 00 00               	movl	$0x7, %ecx
      ba: cd 29                        	int	$0x29
      bc: 0f 0b                        	ud2
      be: 66 90                        	nop

0000000000000000 <willow_panic_depth>:
       0: 55                           	pushq	%rbp
       1: 41 56                        	pushq	%r14
       3: 56                           	pushq	%rsi
       4: 57                           	pushq	%rdi
       5: 53                           	pushq	%rbx
       6: 48 83 ec 30                  	subq	$0x30, %rsp
       a: 48 8d 6c 24 30               	leaq	0x30(%rsp), %rbp
       f: 48 c7 45 f8 fe ff ff ff      	movq	$-0x2, -0x8(%rbp)
      17: 8b 05 00 00 00 00            	movl	(%rip), %eax            # 0x1d <willow_panic_depth+0x1d>
      1d: 65 48 8b 0c 25 58 00 00 00   	movq	%gs:0x58, %rcx
      26: 48 8b 04 c1                  	movq	(%rcx,%rax,8), %rax
      2a: 48 8d b0 00 00 00 00         	leaq	(%rax), %rsi
      31: 0f b6 80 10 00 00 00         	movzbl	0x10(%rax), %eax
      38: 83 f8 01                     	cmpl	$0x1, %eax
      3b: 74 2b                        	je	0x68 <willow_panic_depth+0x68>
      3d: 83 f8 02                     	cmpl	$0x2, %eax
      40: 75 12                        	jne	0x54 <willow_panic_depth+0x54>
      42: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0x49 <willow_panic_depth+0x49>
      49: e8 00 00 00 00               	callq	0x4e <willow_panic_depth+0x4e>
      4e: 90                           	nop
      4f: e9 ce 00 00 00               	jmp	0x122 <willow_panic_depth+0x122>
      54: 48 8d 15 00 00 00 00         	leaq	(%rip), %rdx            # 0x5b <willow_panic_depth+0x5b>
      5b: 48 89 f1                     	movq	%rsi, %rcx
      5e: e8 00 00 00 00               	callq	0x63 <willow_panic_depth+0x63>
      63: 90                           	nop
      64: c6 46 10 01                  	movb	$0x1, 0x10(%rsi)
      68: 48 8b 06                     	movq	(%rsi), %rax
      6b: 48 b9 ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %rcx # imm = 0x7FFFFFFFFFFFFFFF
      75: 48 39 c8                     	cmpq	%rcx, %rax
      78: 0f 83 97 00 00 00            	jae	0x115 <willow_panic_depth+0x115>
      7e: 48 8d 48 01                  	leaq	0x1(%rax), %rcx
      82: 48 89 0e                     	movq	%rcx, (%rsi)
      85: 48 8b 4e 08                  	movq	0x8(%rsi), %rcx
      89: 48 85 c9                     	testq	%rcx, %rcx
      8c: 74 77                        	je	0x105 <willow_panic_depth+0x105>
      8e: f0                           	lock
      8f: 48 ff 01                     	incq	(%rcx)
      92: 0f 8e 8a 00 00 00            	jle	0x122 <willow_panic_depth+0x122>
      98: 48 8b 5e 08                  	movq	0x8(%rsi), %rbx
      9c: 48 ff 0e                     	decq	(%rsi)
      9f: 48 89 5d f0                  	movq	%rbx, -0x10(%rbp)
      a3: 48 8d 73 10                  	leaq	0x10(%rbx), %rsi
      a7: b1 01                        	movb	$0x1, %cl
      a9: 31 c0                        	xorl	%eax, %eax
      ab: f0                           	lock
      ac: 0f b0 4b 10                  	cmpxchgb	%cl, 0x10(%rbx)
      b0: 75 72                        	jne	0x124 <willow_panic_depth+0x124>
      b2: 4c 8b 35 00 00 00 00         	movq	(%rip), %r14            # 0xb9 <willow_panic_depth+0xb9>
      b9: 49 8b 06                     	movq	(%r14), %rax
      bc: 48 d1 e0                     	shlq	%rax
      bf: 48 85 c0                     	testq	%rax, %rax
      c2: 75 6b                        	jne	0x12f <willow_panic_depth+0x12f>
      c4: 0f b6 43 11                  	movzbl	0x11(%rbx), %eax
      c8: 48 8b 7b 28                  	movq	0x28(%rbx), %rdi
      cc: 49 8b 06                     	movq	(%r14), %rax
      cf: 48 d1 e0                     	shlq	%rax
      d2: 48 85 c0                     	testq	%rax, %rax
      d5: 75 77                        	jne	0x14e <willow_panic_depth+0x14e>
      d7: 31 c0                        	xorl	%eax, %eax
      d9: 86 06                        	xchgb	%al, (%rsi)
      db: 3c 02                        	cmpb	$0x2, %al
      dd: 74 64                        	je	0x143 <willow_panic_depth+0x143>
      df: 48 8b 45 f0                  	movq	-0x10(%rbp), %rax
      e3: f0                           	lock
      e4: 48 ff 08                     	decq	(%rax)
      e7: 75 0a                        	jne	0xf3 <willow_panic_depth+0xf3>
      e9: 48 8d 4d f0                  	leaq	-0x10(%rbp), %rcx
      ed: e8 00 00 00 00               	callq	0xf2 <willow_panic_depth+0xf2>
      f2: 90                           	nop
      f3: 48 81 ff ff ff ff 7f         	cmpq	$0x7fffffff, %rdi       # imm = 0x7FFFFFFF
      fa: b8 ff ff ff 7f               	movl	$0x7fffffff, %eax       # imm = 0x7FFFFFFF
      ff: 48 0f 42 c7                  	cmovbq	%rdi, %rax
     103: eb 05                        	jmp	0x10a <willow_panic_depth+0x10a>
     105: 48 89 06                     	movq	%rax, (%rsi)
     108: 31 c0                        	xorl	%eax, %eax
     10a: 48 83 c4 30                  	addq	$0x30, %rsp
     10e: 5b                           	popq	%rbx
     10f: 5f                           	popq	%rdi
     110: 5e                           	popq	%rsi
     111: 41 5e                        	popq	%r14
     113: 5d                           	popq	%rbp
     114: c3                           	retq
     115: 48 8d 0d 00 00 00 00         	leaq	(%rip), %rcx            # 0x11c <willow_panic_depth+0x11c>
     11c: e8 00 00 00 00               	callq	0x121 <willow_panic_depth+0x121>
     121: 90                           	nop
     122: 0f 0b                        	ud2
     124: 48 89 f1                     	movq	%rsi, %rcx
     127: e8 00 00 00 00               	callq	0x12c <willow_panic_depth+0x12c>
     12c: 90                           	nop
     12d: eb 83                        	jmp	0xb2 <willow_panic_depth+0xb2>
     12f: e8 00 00 00 00               	callq	0x134 <willow_panic_depth+0x134>
     134: 90                           	nop
     135: 0f b6 4b 11                  	movzbl	0x11(%rbx), %ecx
     139: 48 8b 7b 28                  	movq	0x28(%rbx), %rdi
     13d: 84 c0                        	testb	%al, %al
     13f: 75 8b                        	jne	0xcc <willow_panic_depth+0xcc>
     141: eb 94                        	jmp	0xd7 <willow_panic_depth+0xd7>
     143: 48 89 f1                     	movq	%rsi, %rcx
     146: e8 00 00 00 00               	callq	0x14b <willow_panic_depth+0x14b>
     14b: 90                           	nop
     14c: eb 91                        	jmp	0xdf <willow_panic_depth+0xdf>
     14e: e8 00 00 00 00               	callq	0x153 <willow_panic_depth+0x153>
     153: 90                           	nop
     154: 84 c0                        	testb	%al, %al
     156: 0f 85 7b ff ff ff            	jne	0xd7 <willow_panic_depth+0xd7>
     15c: c6 43 11 01                  	movb	$0x1, 0x11(%rbx)
     160: e9 72 ff ff ff               	jmp	0xd7 <willow_panic_depth+0xd7>
     165: 66 66 2e 0f 1f 84 00 00 00 00 00     	nopw	%cs:(%rax,%rax)
