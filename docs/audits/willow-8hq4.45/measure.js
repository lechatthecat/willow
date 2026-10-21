// Reconstructed, public ticket workload. No historical-source identity claimed.
const {spawn, spawnSync} = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const root = path.resolve(__dirname, '../../..');
const java = process.env.JAVA_HOME ? path.join(process.env.JAVA_HOME,'bin','java') :
 (fs.existsSync('/usr/lib/jvm/java-26-openjdk-amd64/bin/java') ? '/usr/lib/jvm/java-26-openjdk-amd64/bin/java' : 'java');
const ti = process.argv.indexOf('--trials');
const trials = ti < 0 ? 5 : Number(process.argv[ti + 1]);
if (!Number.isInteger(trials) || trials < 1) throw Error('invalid trials');
for(let i=2;i<process.argv.length;i++) {
 if(process.argv[i]==='--trials') { i++; continue; }
 if(!['--no-build','word_count','map_lookup_insert'].includes(process.argv[i])) throw Error('usage: measure.js [--trials N] [--no-build]');
}
function build(cmd, args) {
 const r = spawnSync(cmd,args,{cwd:root,encoding:'utf8'});
 if(r.status !== 0) throw Error(r.error || r.stderr || r.stdout);
 return r.stdout.trim();
}
async function trial(binary, workers, args = []) {
 return new Promise((resolve,reject) => {
  const child = spawn(binary, args, {cwd:root,env:{...process.env,WILLOW_WORKERS:String(workers),GOMAXPROCS:String(workers)}});
  let buffer='', start, elapsed, starts=0, ends=0, last='', error='';
  const timer=setTimeout(()=>child.kill('SIGKILL'),120000);
  child.stdout.on('data', d => {
   buffer+=d; let p;
   while((p=buffer.indexOf('\n'))>=0) {
    const line=buffer.slice(0,p).trim(); buffer=buffer.slice(p+1); last=line;
    if(line==='@start work') { start=process.hrtime.bigint(); starts++; }
    if(line==='@end work') { if(start) elapsed=Number(process.hrtime.bigint()-start)/1e6; ends++; }
   }
  });
  child.stderr.on('data',d=>error+=d);
  child.on('error',reject);
  child.on('close',code=>{clearTimeout(timer);
   if(code || starts!==1 || ends!==1 || !(elapsed>0) || last!==(args.at(-1)==='map_lookup_insert' || binary.includes('map_lookup_insert') ? '17999994000000' : '6000000')) reject(Error(JSON.stringify({code,last,error,elapsed})));
   else resolve(elapsed);
  });
 });
}
(async()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'map-bench-'));
 try {
  const suffix=process.platform==='win32'?'.exe':'';
  if(!process.argv.includes('--no-build')) build('cargo',['build','--release','--bin','willow']);
  const go=path.join(dir,'go'+suffix);
  build('go',['build','-o',go,path.join(__dirname,'maps.go')]);
  build(java,['-m','jdk.compiler/com.sun.tools.javac.Main','-d',dir,path.join(__dirname,'Maps.java')]);
  const metadata={head:build('git',['rev-parse','HEAD']),platform:process.platform,arch:process.arch,cpu:os.cpus()[0].model,
   reconstructed:true,go:build('go',['version']),java:build(java,['--version']),
   diff_sha256:crypto.createHash('sha256').update(build('git',['diff','--','crates'])).digest('hex'),
   hashes:Object.fromEntries(['word_count.wi','map_lookup_insert.wi','maps.go','Maps.java','measure.js'].map(f=>[f,crypto.createHash('sha256').update(fs.readFileSync(path.join(__dirname,f))).digest('hex')]))};
  for(const name of process.argv.slice(2).filter(a=>['word_count','map_lookup_insert'].includes(a))) {
   const willow=path.join(dir,name+suffix);
   build('target/release/willow',['build',path.join(__dirname,name+'.wi'),'-o',willow,'--release']);
   const times={willow:[],go:[],java:[]};
   for(let i=0;i<trials;i++) for(const lang of i%2?['java','go','willow']:['willow','go','java']) {
    times[lang].push(await trial(lang==='willow'?willow:lang==='go'?go:java,8,lang==='java'?['-XX:ActiveProcessorCount=8','-cp',dir,'Maps',name]:lang==='go'?[name]:[]));
   }
   for(const [language,ms] of Object.entries(times)) {
    const sorted=[...ms].sort((a,b)=>a-b),median=(sorted[Math.floor((trials-1)/2)]+sorted[Math.floor(trials/2)])/2;
    console.log(JSON.stringify({...metadata,case:name,language,trials,ms,median_ms:median,ns_per_operation:median/(name==='word_count'?6:15)}));
   }
  }
 } finally {fs.rmSync(dir,{recursive:true,force:true});}
})().catch(e=>{console.error(e);process.exitCode=1;});
