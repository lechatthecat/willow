00000000000197b0 <_willow_root_depth>:
   197b0: 55                           	pushq	%rbp
   197b1: 48 89 e5                     	movq	%rsp, %rbp
   197b4: 53                           	pushq	%rbx
   197b5: 50                           	pushq	%rax
   197b6: 48 8b 3d 00 00 00 00         	movq	(%rip), %rdi            ## 0x197bd <_willow_root_depth+0xd>
   197bd: ff 17                        	callq	*(%rdi)
   197bf: 48 89 c3                     	movq	%rax, %rbx
   197c2: 0f b6 40 20                  	movzbl	0x20(%rax), %eax
   197c6: 83 f8 01                     	cmpl	$0x1, %eax
   197c9: 74 26                        	je	0x197f1 <_willow_root_depth+0x41>
   197cb: 83 f8 02                     	cmpl	$0x2, %eax
   197ce: 75 0e                        	jne	0x197de <_willow_root_depth+0x2e>
   197d0: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x197d7 <_willow_root_depth+0x27>
   197d7: e8 00 00 00 00               	callq	0x197dc <_willow_root_depth+0x2c>
   197dc: eb 66                        	jmp	0x19844 <_willow_root_depth+0x94>
   197de: 48 8d 35 00 00 00 00         	leaq	(%rip), %rsi            ## 0x197e5 <_willow_root_depth+0x35>
   197e5: 48 89 df                     	movq	%rbx, %rdi
   197e8: e8 00 00 00 00               	callq	0x197ed <_willow_root_depth+0x3d>
   197ed: c6 43 20 01                  	movb	$0x1, 0x20(%rbx)
   197f1: 48 8b 0b                     	movq	(%rbx), %rcx
   197f4: 48 b8 ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %rax ## imm = 0x7FFFFFFFFFFFFFFF
   197fe: 48 39 c1                     	cmpq	%rax, %rcx
   19801: 73 1d                        	jae	0x19820 <_willow_root_depth+0x70>
   19803: 48 8d 41 01                  	leaq	0x1(%rcx), %rax
   19807: 48 89 03                     	movq	%rax, (%rbx)
   1980a: 48 8b 43 18                  	movq	0x18(%rbx), %rax
   1980e: 48 a9 00 00 00 80            	testq	$-0x80000000, %rax      ## imm = 0x80000000
   19814: 75 18                        	jne	0x1982e <_willow_root_depth+0x7e>
   19816: 48 89 0b                     	movq	%rcx, (%rbx)
   19819: 48 83 c4 08                  	addq	$0x8, %rsp
   1981d: 5b                           	popq	%rbx
   1981e: 5d                           	popq	%rbp
   1981f: c3                           	retq
   19820: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x19827 <_willow_root_depth+0x77>
   19827: e8 00 00 00 00               	callq	0x1982c <_willow_root_depth+0x7c>
   1982c: eb 16                        	jmp	0x19844 <_willow_root_depth+0x94>
   1982e: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x19835 <_willow_root_depth+0x85>
   19835: be 65 00 00 00               	movl	$0x65, %esi
   1983a: e8 00 00 00 00               	callq	0x1983f <_willow_root_depth+0x8f>
   1983f: e8 00 00 00 00               	callq	0x19844 <_willow_root_depth+0x94>
   19844: 0f 0b                        	ud2
   19846: 48 ff 0b                     	decq	(%rbx)
   19849: e8 00 00 00 00               	callq	0x1984e <_willow_root_depth+0x9e>
   1984e: e8 00 00 00 00               	callq	0x19853 <_willow_root_depth+0xa3>

0000000000004a70 <_willow_panic_depth>:
    4a70: 55                           	pushq	%rbp
    4a71: 48 89 e5                     	movq	%rsp, %rbp
    4a74: 41 57                        	pushq	%r15
    4a76: 41 56                        	pushq	%r14
    4a78: 41 55                        	pushq	%r13
    4a7a: 41 54                        	pushq	%r12
    4a7c: 53                           	pushq	%rbx
    4a7d: 50                           	pushq	%rax
    4a7e: 48 8b 3d 00 00 00 00         	movq	(%rip), %rdi            ## 0x4a85 <_willow_panic_depth+0x15>
    4a85: ff 17                        	callq	*(%rdi)
    4a87: 0f b6 48 10                  	movzbl	0x10(%rax), %ecx
    4a8b: 83 f9 01                     	cmpl	$0x1, %ecx
    4a8e: 74 2f                        	je	0x4abf <_willow_panic_depth+0x4f>
    4a90: 83 f9 02                     	cmpl	$0x2, %ecx
    4a93: 75 11                        	jne	0x4aa6 <_willow_panic_depth+0x36>
    4a95: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x4a9c <_willow_panic_depth+0x2c>
    4a9c: e8 00 00 00 00               	callq	0x4aa1 <_willow_panic_depth+0x31>
    4aa1: e9 d5 00 00 00               	jmp	0x4b7b <_willow_panic_depth+0x10b>
    4aa6: 48 8d 35 00 00 00 00         	leaq	(%rip), %rsi            ## 0x4aad <_willow_panic_depth+0x3d>
    4aad: 48 89 c3                     	movq	%rax, %rbx
    4ab0: 48 89 c7                     	movq	%rax, %rdi
    4ab3: e8 00 00 00 00               	callq	0x4ab8 <_willow_panic_depth+0x48>
    4ab8: 48 89 d8                     	movq	%rbx, %rax
    4abb: c6 43 10 01                  	movb	$0x1, 0x10(%rbx)
    4abf: 49 be ff ff ff ff ff ff ff 7f	movabsq	$0x7fffffffffffffff, %r14 ## imm = 0x7FFFFFFFFFFFFFFF
    4ac9: 48 8b 08                     	movq	(%rax), %rcx
    4acc: 4c 39 f1                     	cmpq	%r14, %rcx
    4acf: 0f 83 9a 00 00 00            	jae	0x4b6f <_willow_panic_depth+0xff>
    4ad5: 48 8d 51 01                  	leaq	0x1(%rcx), %rdx
    4ad9: 48 89 10                     	movq	%rdx, (%rax)
    4adc: 48 8b 50 08                  	movq	0x8(%rax), %rdx
    4ae0: 48 85 d2                     	testq	%rdx, %rdx
    4ae3: 74 76                        	je	0x4b5b <_willow_panic_depth+0xeb>
    4ae5: f0                           	lock
    4ae6: 48 ff 02                     	incq	(%rdx)
    4ae9: 0f 8e 8c 00 00 00            	jle	0x4b7b <_willow_panic_depth+0x10b>
    4aef: 4c 8b 78 08                  	movq	0x8(%rax), %r15
    4af3: 48 ff 08                     	decq	(%rax)
    4af6: 4c 89 7d d0                  	movq	%r15, -0x30(%rbp)
    4afa: 49 8d 5f 18                  	leaq	0x18(%r15), %rbx
    4afe: 49 8b 7f 18                  	movq	0x18(%r15), %rdi
    4b02: 48 85 ff                     	testq	%rdi, %rdi
    4b05: 74 76                        	je	0x4b7d <_willow_panic_depth+0x10d>
    4b07: e8 00 00 00 00               	callq	0x4b0c <_willow_panic_depth+0x9c>
    4b0c: 4c 8b 2d 00 00 00 00         	movq	(%rip), %r13            ## 0x4b13 <_willow_panic_depth+0xa3>
    4b13: 49 8b 45 00                  	movq	(%r13), %rax
    4b17: 4c 85 f0                     	testq	%r14, %rax
    4b1a: 75 71                        	jne	0x4b8d <_willow_panic_depth+0x11d>
    4b1c: 41 0f b6 47 20               	movzbl	0x20(%r15), %eax
    4b21: 4d 8b 67 38                  	movq	0x38(%r15), %r12
    4b25: 49 8b 45 00                  	movq	(%r13), %rax
    4b29: 4c 85 f0                     	testq	%r14, %rax
    4b2c: 75 73                        	jne	0x4ba1 <_willow_panic_depth+0x131>
    4b2e: 48 8b 3b                     	movq	(%rbx), %rdi
    4b31: e8 00 00 00 00               	callq	0x4b36 <_willow_panic_depth+0xc6>
    4b36: 48 8b 45 d0                  	movq	-0x30(%rbp), %rax
    4b3a: f0                           	lock
    4b3b: 48 ff 08                     	decq	(%rax)
    4b3e: 75 09                        	jne	0x4b49 <_willow_panic_depth+0xd9>
    4b40: 48 8d 7d d0                  	leaq	-0x30(%rbp), %rdi
    4b44: e8 00 00 00 00               	callq	0x4b49 <_willow_panic_depth+0xd9>
    4b49: 49 81 fc ff ff ff 7f         	cmpq	$0x7fffffff, %r12       ## imm = 0x7FFFFFFF
    4b50: b8 ff ff ff 7f               	movl	$0x7fffffff, %eax       ## imm = 0x7FFFFFFF
    4b55: 49 0f 42 c4                  	cmovbq	%r12, %rax
    4b59: eb 05                        	jmp	0x4b60 <_willow_panic_depth+0xf0>
    4b5b: 48 89 08                     	movq	%rcx, (%rax)
    4b5e: 31 c0                        	xorl	%eax, %eax
    4b60: 48 83 c4 08                  	addq	$0x8, %rsp
    4b64: 5b                           	popq	%rbx
    4b65: 41 5c                        	popq	%r12
    4b67: 41 5d                        	popq	%r13
    4b69: 41 5e                        	popq	%r14
    4b6b: 41 5f                        	popq	%r15
    4b6d: 5d                           	popq	%rbp
    4b6e: c3                           	retq
    4b6f: 48 8d 3d 00 00 00 00         	leaq	(%rip), %rdi            ## 0x4b76 <_willow_panic_depth+0x106>
    4b76: e8 00 00 00 00               	callq	0x4b7b <_willow_panic_depth+0x10b>
    4b7b: 0f 0b                        	ud2
    4b7d: 48 89 df                     	movq	%rbx, %rdi
    4b80: e8 00 00 00 00               	callq	0x4b85 <_willow_panic_depth+0x115>
    4b85: 48 89 c7                     	movq	%rax, %rdi
    4b88: e9 7a ff ff ff               	jmp	0x4b07 <_willow_panic_depth+0x97>
    4b8d: e8 00 00 00 00               	callq	0x4b92 <_willow_panic_depth+0x122>
    4b92: 41 0f b6 4f 20               	movzbl	0x20(%r15), %ecx
    4b97: 4d 8b 67 38                  	movq	0x38(%r15), %r12
    4b9b: 84 c0                        	testb	%al, %al
    4b9d: 75 86                        	jne	0x4b25 <_willow_panic_depth+0xb5>
    4b9f: eb 8d                        	jmp	0x4b2e <_willow_panic_depth+0xbe>
    4ba1: e8 00 00 00 00               	callq	0x4ba6 <_willow_panic_depth+0x136>
    4ba6: 84 c0                        	testb	%al, %al
    4ba8: 75 84                        	jne	0x4b2e <_willow_panic_depth+0xbe>
    4baa: 41 c6 47 20 01               	movb	$0x1, 0x20(%r15)
    4baf: e9 7a ff ff ff               	jmp	0x4b2e <_willow_panic_depth+0xbe>
    4bb4: e8 00 00 00 00               	callq	0x4bb9 <_willow_panic_depth+0x149>
    4bb9: 48 8b 45 d0                  	movq	-0x30(%rbp), %rax
    4bbd: f0                           	lock
    4bbe: 48 ff 08                     	decq	(%rax)
    4bc1: 75 09                        	jne	0x4bcc <_willow_panic_depth+0x15c>
    4bc3: 48 8d 7d d0                  	leaq	-0x30(%rbp), %rdi
    4bc7: e8 00 00 00 00               	callq	0x4bcc <_willow_panic_depth+0x15c>
    4bcc: e8 00 00 00 00               	callq	0x4bd1 <_willow_panic_depth+0x161>
    4bd1: e8 00 00 00 00               	callq	0x4bd6 <_willow_panic_depth+0x166>
    4bd6: e8 00 00 00 00               	callq	0x4bdb <_willow_panic_depth+0x16b>
    4bdb: 0f 1f 44 00 00               	nopl	(%rax,%rax)
