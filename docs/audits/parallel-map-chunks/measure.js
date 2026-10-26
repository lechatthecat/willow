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
 if(process.argv[i]!=='--no-build') throw Error('usage: measure.js [--trials N] [--no-build]');
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
   if(code || starts!==1 || ends!==1 || !(elapsed>0) || last!=='131070000000') reject(Error(JSON.stringify({code,last,error,elapsed})));
   else resolve(elapsed);
  });
 });
}
(async()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'parallel-map-'));
 try {
  const suffix=process.platform==='win32'?'.exe':'';
  const binaries={willow:path.join(dir,'willow'+suffix),go:path.join(dir,'go'+suffix),java};
  if(!process.argv.includes('--no-build')) build('cargo',['build','--release','--bin','willow']);
  build('target/release/willow',['build',path.join(__dirname,'parallel_map.wi'),'-o',binaries.willow,'--release']);
  build('go',['build','-o',binaries.go,path.join(__dirname,'parallel.go')]);
  build(java,['-m','jdk.compiler/com.sun.tools.javac.Main','-d',dir,path.join(__dirname,'Parallel.java')]);
  const oneSource=path.join(dir,'one.wi'), oneBinary=path.join(dir,'one'+suffix);
  fs.writeFileSync(oneSource,fs.readFileSync(path.join(__dirname,'parallel_map.wi'),'utf8').replace('round < 5','round < 1'));
  build('target/release/willow',['build',oneSource,'-o',oneBinary,'--release']);
  const metadata={head:build('git',['rev-parse','HEAD']),platform:process.platform,arch:process.arch,cpu:os.cpus()[0].model,
   reconstructed:true,go:build('go',['version']),java:build(java,['--version']),
   diff_sha256:crypto.createHash('sha256').update(build('git',['diff','--','crates'])).digest('hex'),
   hashes:Object.fromEntries(['parallel_map.wi','parallel.go','Parallel.java','measure.js'].map(f=>[f,crypto.createHash('sha256').update(fs.readFileSync(path.join(__dirname,f))).digest('hex')]))};
  for(const workers of [8,1]) {
   const results={willow:[],go:[],java:[]};
   for(let i=0;i<trials;i++) for(const language of i%2?['java','go','willow']:['willow','go','java']) results[language].push(await trial(binaries[language],workers,language==='java'?['-XX:ActiveProcessorCount='+workers,'-cp',dir,'Parallel']:[]));
   for(const [language,ms] of Object.entries(results)) {
    const sorted=[...ms].sort((a,b)=>a-b), median=(sorted[Math.floor((trials-1)/2)]+sorted[Math.floor(trials/2)])/2;
    console.log(JSON.stringify({...metadata,language,workers,rounds:5,trials,ms,median_ms:median,ns_per_element:median/20}));
   }
  }
  const ms=[];
  for(let i=0;i<trials;i++) ms.push(await trial(oneBinary,1));
  const sorted=[...ms].sort((a,b)=>a-b), median=(sorted[Math.floor((trials-1)/2)]+sorted[Math.floor(trials/2)])/2;
  console.log(JSON.stringify({...metadata,language:'willow',workers:1,rounds:1,trials,ms,median_ms:median,ns_per_element:median/4}));
 } finally {fs.rmSync(dir,{recursive:true,force:true});}
})().catch(e=>{console.error(e);process.exitCode=1;});
