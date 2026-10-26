// New four-way split for allocation throughput audit. Source checksums prevent elision.
const {spawn,spawnSync}=require('node:child_process');
const fs=require('node:fs');
const path=require('node:path');
const root=path.resolve(__dirname,'../../../..');
const out=path.join(root,'target/runtime_compare/allocation');fs.mkdirSync(out,{recursive:true});
const modes=process.argv[2]?[process.argv[2]]:['scalar','none','edge','chain'];
const ns=process.argv[3]?[Number(process.argv[3])]:[300000,1000000,3000000];
if(modes.some(m=>!['scalar','none','edge','chain'].includes(m))||ns.some(n=>![300000,1000000,3000000].includes(n)))throw Error('invalid probe selection');
const records=[];
(async()=>{
 for(const n of ns) for(const mode of modes){
 const name=mode+'_'+n, bin=path.join(out,name);
 const r=spawnSync(path.join(root,'target/release/willow'),['build',path.join(__dirname,name+'.wi'),'--release','-o',bin],{encoding:'utf8'});
 if(r.status!==0)throw Error(r.stderr+r.stdout);
 for(let trial=1;trial<=5;trial++){
 const row=await new Promise((resolve,reject)=>{
 const p=spawn(bin,[],{env:{...process.env,WILLOW_WORKERS:'8'}});let buffer='',lines=[],start,end,err='';
 const timer=setTimeout(()=>{p.kill();reject(Error('timeout '+name));},120000);
 p.stdout.on('data',d=>{buffer+=d;let i;while((i=buffer.indexOf('\n'))>=0){const l=buffer.slice(0,i).trim();buffer=buffer.slice(i+1);lines.push(l);if(l==='@start')start=process.hrtime.bigint();if(l==='@end')end=process.hrtime.bigint();}});
 p.stderr.on('data',d=>err+=d);p.on('error',reject);
 p.on('close',code=>{clearTimeout(timer);const k=mode==='scalar'?Math.max(0,n-1024):n;const expected=k*(k-1)/2+(mode==='edge'?7*n:0);
 if(code!==0||!start||!end||Number(lines[2])!==expected)return reject(Error(name+': '+code+' '+lines+' '+err));
 resolve({mode,n,trial,ns:Number(end-start),ns_per_object:Number(end-start)/n,minor:Number(lines[3]),major:Number(lines[4]),moved:Number(lines[5]),promoted:Number(lines[6])});});
 }); records.push(row);console.log(JSON.stringify(row));
 }
 }
 for(const n of ns)for(const mode of modes){const rows=records.filter(r=>r.n===n&&r.mode===mode).map(r=>r.ns_per_object).sort((a,b)=>a-b);console.log(JSON.stringify({summary:true,mode,n,median:rows[2],spread:(rows[4]-rows[0])/rows[2]}));}
})().catch(e=>{console.error(e);process.exitCode=1;});
