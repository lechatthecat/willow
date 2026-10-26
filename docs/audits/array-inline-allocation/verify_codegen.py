#!/usr/bin/env python3
"""Linux x86_64 gdb proof of emitted fast allocation, boundary and fallback paths.
Boundary cases fill the TLAB with real generated array allocations. Checks
returned values and runs a minor collection afterwards. No GC state is modified.
"""
import ast, pathlib, subprocess, tempfile, re
compiler=pathlib.Path("target/debug/willow").resolve()
symbols=["willow_array_new_literal", "willow_array_new", "willow_panic_depth", "willow_array_set"]
with tempfile.TemporaryDirectory() as tmp:
    root=pathlib.Path(tmp)
    for length in [0, 3, 509, 510]:
        # 16-byte header + 8-byte tracing prefix + 509 words = 4096 bytes.
        total=48+16+8+length*8
        expr="["+", ".join(["i"]*length)+"]"
        source=root/"main.wi"; binary=root/"probe"
        template=f"""import std::collections::Array;
fn literal(i: i64) -> Array<i64> {{ return {expr}; }}
fn main() {{ let warm = literal(7); FILLER let a = literal(42); gc_minor_collect();
println(a.len()); {"println(a[0]);" if length else ""} }}
"""
        source.write_text(template.replace("FILLER", ""))
        subprocess.run([str(compiler),"build",str(source),"--release","-o",str(binary)],check=True,capture_output=True)
        remaining=None
        for mode in (["normal","exact","short"] if length<=509 else ["large"]):
            if mode in ["exact", "short"]:
                padding=remaining-(total if mode=="exact" else total-8)
                chunks=[]
                while padding>4144:
                    # Leave at least one minimum 72-byte array for the tail.
                    amount=min(4144,padding-72)
                    chunks.append(amount); padding-=amount
                if padding: chunks.append(padding)
                helpers="".join(f"fn pad{amount}() -> Array<i64> {{ return ["+",".join(["0"]*((amount-72)//8))+"]; }" for amount in sorted(set(chunks)))
                filler="".join(f"let pad{index}=pad{amount}();" for index,amount in enumerate(chunks))
                assert all(amount>=72 and amount%8==0 for amount in chunks)
                source.write_text(template.replace("FILLER",filler)+helpers)
                subprocess.run([str(compiler),"build",str(source),"--release","-o",str(binary)],check=True,capture_output=True)
            disasm=subprocess.check_output(["objdump","-d","--disassemble=literal",str(binary)],text=True)
            call_sites=dict(re.findall(r"^\s*([0-9a-f]+):.*\bcall[q]?\s+[0-9a-f]+ <([^>]+)>",disasm,re.M))
            script=root/"count.gdb"
            script.write_text("""set pagination off
set confirm off
python
import gdb, struct
counts = {}
active = False
class Counter(gdb.Breakpoint):
    def __init__(self,name):
        super().__init__(name, internal=True); self.name=name; counts[name]=0
    def stop(self):
        if active: counts[self.name]+=1
        return False
class Entry(gdb.Breakpoint):
    def __init__(self):
        super().__init__('literal', internal=True); self.calls=0
    def stop(self):
        global active
        self.calls+=1
        if self.calls==2:
            active=True
            context=int(gdb.parse_and_eval('$r15'))
            memory=gdb.selected_inferior()
            tlab=struct.unpack('<Q',memory.read_memory(context+8,8))[0]
            cursor,limit=struct.unpack('<QQ',memory.read_memory(tlab,16))
            print('REMAINING '+str(limit-cursor))
"""+ (f"            assert limit-cursor == {total if mode=='exact' else total-8}\n" if mode in ["exact","short"] else "")+"""        return False
Entry()
"""+"\n".join(f"Counter({symbol!r})" for symbol in symbols + ["*0x"+address for address in call_sites])+"""
end
run
python
print('COUNTS '+repr(counts))
end
""")
            result=subprocess.run(["gdb","--batch","-x",str(script),str(binary)],check=True,capture_output=True,text=True)
            assert "Traceback" not in result.stderr, result.stderr
            if mode=="normal":
                remaining=int(next(line[10:] for line in result.stdout.splitlines() if line.startswith("REMAINING ")))
            counts=ast.literal_eval(next(line[7:] for line in result.stdout.splitlines() if line.startswith("COUNTS ")))
            expected=int(mode in ["short","large"])
            allocation_counts={s:counts[s] for s in symbols}
            executed_calls=sum(counts["*0x"+address] for address in call_sites)
            setup={"willow_sync_poll_counter", "willow_gc_stop_flag"}
            allocation_region_calls=sum(counts["*0x"+address] for address,target in call_sites.items() if target not in setup)
            if mode in ["normal","exact"]: assert allocation_region_calls==0,counts
            assert allocation_counts=={s:expected if s==symbols[0] else 0 for s in symbols},result.stdout+result.stderr
            assert "exited normally" in result.stdout,result.stdout
            assert f"\n{length}\n" in result.stdout,result.stdout
            if length: assert "\n42\n" in result.stdout,result.stdout
            print(f"length={length} mode={mode} allocation_counts={allocation_counts} non_setup_executed_calls={allocation_region_calls} function_setup_calls={executed_calls-allocation_region_calls}",flush=True)
    for sites in [1,8,32,128]:
        source.write_text("import std::collections::Array; fn batch() -> i64 { let mut sum=0;"+
            "".join(f"let a{i}: Array<i64>=[]; sum=sum+a{i}.len();" for i in range(sites))+"return sum; } fn main(){ println(batch()); }")
        subprocess.run([str(compiler),"build",str(source),"--release","-o",str(binary)],check=True,capture_output=True)
        nm=subprocess.check_output(["nm","-S",str(binary)],text=True)
        size=int(next(line.split()[1] for line in nm.splitlines() if line.endswith(" batch")),16)
        disasm=subprocess.check_output(["objdump","-d","--disassemble=batch",str(binary)],text=True)
        calls=disasm.count("<willow_array_new_literal>")
        assert calls==sites,(sites,calls)
        print(f"sites={sites} batch_bytes={size} slow_call_sites={calls}",flush=True)
