00000000000197a0 <_willow_root_depth>:
   197a0: 55                           	pushq	%rbp
   197a1: 48 89 e5                     	movq	%rsp, %rbp
   197a4: 48 8b 3d 00 00 00 00         	movq	(%rip), %rdi            ## 0x197ab <_willow_root_depth+0xb>
   197ab: ff 17                        	callq	*(%rdi)
   197ad: 48 8b 00                     	movq	(%rax), %rax
   197b0: 48 3d ff ff ff 7f            	cmpq	$0x7fffffff, %rax       ## imm = 0x7FFFFFFF
   197b6: 77 02                        	ja	0x197ba <_willow_root_depth+0x1a>
   197b8: 5d                           	popq	%rbp
   197b9: c3                           	retq
   197ba: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x197c1 <_willow_root_depth+0x21>
   197c1: be 65 00 00 00               	movl	$0x65, %esi
   197c6: e8 00 00 00 00               	callq	0x197cb <_willow_root_depth+0x2b>
   197cb: e8 00 00 00 00               	callq	0x197d0 <_willow_root_depth+0x30>
   197d0: 0f 0b                        	ud2
   197d2: e8 00 00 00 00               	callq	0x197d7 <_willow_root_depth+0x37>

0000000000008450 <_willow_panic_depth>:
    8450: 55                           	pushq	%rbp
    8451: 48 89 e5                     	movq	%rsp, %rbp
    8454: 53                           	pushq	%rbx
    8455: 50                           	pushq	%rax
    8456: 48 8b 3d 00 00 00 00         	movq	(%rip), %rdi            ## 0x845d <_willow_panic_depth+0xd>
    845d: ff 17                        	callq	*(%rdi)
    845f: 0f b6 48 10                  	movzbl	0x10(%rax), %ecx
    8463: 83 f9 01                     	cmpl	$0x1, %ecx
    8466: 74 2c                        	je	0x8494 <_willow_panic_depth+0x44>
    8468: 83 f9 02                     	cmpl	$0x2, %ecx
    846b: 75 0e                        	jne	0x847b <_willow_panic_depth+0x2b>
    846d: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x8474 <_willow_panic_depth+0x24>
    8474: e8 00 00 00 00               	callq	0x8479 <_willow_panic_depth+0x29>
    8479: eb 71                        	jmp	0x84ec <_willow_panic_depth+0x9c>
    847b: 48 8d 35 00 00 00 00         	leaq	(%rip), %rsi            ## 0x8482 <_willow_panic_depth+0x32>
    8482: 48 89 c3                     	movq	%rax, %rbx
    8485: 48 89 c7                     	movq	%rax, %rdi
    8488: e8 00 00 00 00               	callq	0x848d <_willow_panic_depth+0x3d>
    848d: 48 89 d8                     	movq	%rbx, %rax
    8490: c6 43 10 01                  	movb	$0x1, 0x10(%rbx)
    8494: 48 8b 10                     	movq	(%rax), %rdx
    8497: 48 b9 ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %rcx ## imm = 0x7FFFFFFFFFFFFFFF
    84a1: 48 39 ca                     	cmpq	%rcx, %rdx
    84a4: 73 3a                        	jae	0x84e0 <_willow_panic_depth+0x90>
    84a6: 48 8d 4a 01                  	leaq	0x1(%rdx), %rcx
    84aa: 48 89 08                     	movq	%rcx, (%rax)
    84ad: 48 8b 48 08                  	movq	0x8(%rax), %rcx
    84b1: 48 85 c9                     	testq	%rcx, %rcx
    84b4: 74 1c                        	je	0x84d2 <_willow_panic_depth+0x82>
    84b6: 48 8b 51 60                  	movq	0x60(%rcx), %rdx
    84ba: 48 81 fa ff ff ff 7f         	cmpq	$0x7fffffff, %rdx       ## imm = 0x7FFFFFFF
    84c1: b9 ff ff ff 7f               	movl	$0x7fffffff, %ecx       ## imm = 0x7FFFFFFF
    84c6: 48 0f 42 ca                  	cmovbq	%rdx, %rcx
    84ca: 48 8b 10                     	movq	(%rax), %rdx
    84cd: 48 ff ca                     	decq	%rdx
    84d0: eb 02                        	jmp	0x84d4 <_willow_panic_depth+0x84>
    84d2: 31 c9                        	xorl	%ecx, %ecx
    84d4: 48 89 10                     	movq	%rdx, (%rax)
    84d7: 89 c8                        	movl	%ecx, %eax
    84d9: 48 83 c4 08                  	addq	$0x8, %rsp
    84dd: 5b                           	popq	%rbx
    84de: 5d                           	popq	%rbp
    84df: c3                           	retq
    84e0: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x84e7 <_willow_panic_depth+0x97>
    84e7: e8 00 00 00 00               	callq	0x84ec <_willow_panic_depth+0x9c>
    84ec: 0f 0b                        	ud2
    84ee: e8 00 00 00 00               	callq	0x84f3 <_willow_panic_depth+0xa3>
    84f3: 66 2e 0f 1f 84 00 00 00 00 00	nopw	%cs:(%rax,%rax)
    84fd: 0f 1f 00                     	nopl	(%rax)
