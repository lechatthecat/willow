// Newly reconstructed acceptance harness. Phase timing excludes process startup.
// Mutex cases compare equivalent increments, launch/join scope and checked sums.
const {spawn, spawnSync} = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const crypto = require('node:crypto');
// This public, reproducible case lives beside its audit evidence. Preserve the
// existing reconstructed cases and the ticket's documented entry point.
if (process.argv[2] === 'run' && process.argv[3] === 'parallel_map') {
  const result = spawnSync(process.execPath,
    [path.resolve(__dirname, '../../docs/audits/willow-8hq4.46/measure.js'), ...process.argv.slice(4)],
    {stdio: 'inherit'});
  if (result.error) throw result.error;
  process.exit(result.status ?? 1);
}
if (process.argv[2] === 'run' && ['word_count','map_lookup_insert'].includes(process.argv[3])) {
  const result = spawnSync(process.execPath,
    [path.resolve(__dirname, '../../docs/audits/willow-8hq4.45/measure.js'), ...process.argv.slice(3)],
    {stdio: 'inherit'});
  if (result.error) throw result.error;
  process.exit(result.status ?? 1);
}
const args = process.argv.slice(2);
if(args.shift() !== 'run') throw Error('usage: harness.js run CASE... --trials N');
let trials = 5; const ti = args.indexOf('--trials');
if(ti >= 0) { trials = Number(args[ti+1]); args.splice(ti,2); }
const expected = {mutex_contention:'1600000',mutex_split:'1600000',atomic_counter:'400000',producer_consumer:'19999900000',gc_scheduler:'10000600000'};
if (!Number.isInteger(trials) || trials < 1) throw Error('trials must be positive integer');
if(args.includes('mutex_contention') && !args.includes('mutex_split')) args.push('mutex_split');
const dir = fs.mkdtempSync(path.join(os.tmpdir(),'willow-8hq4-42-'));
async function trial(binary, name, language) {
  return new Promise((resolve,reject) => {
    const child = spawn(binary, language === 'go' ? [name] : [], {env:{...process.env,WILLOW_WORKERS:'8'}});
    let buffer='', lines=[], start, elapsed, error='', starts=0, ends=0;
    const timer=setTimeout(()=>{child.kill('SIGKILL'); reject(Error('timeout '+name));},120000);
    child.stderr.on('data',d=>error+=d);
    child.stdout.on('data',d=> {buffer+=d; let pos;
      while((pos=buffer.indexOf('\n'))>=0) {
        const line=buffer.slice(0,pos).trim(); buffer=buffer.slice(pos+1); lines.push(line);
        const now=process.hrtime.bigint();
        if(line==='@start work') { start=now; starts++; }
        if(line==='@end work') { if(start===undefined) return; elapsed=Number(now-start)/1e6; ends++; }
      }
    });
    child.on('error',reject);
    child.on('close',code=>{clearTimeout(timer);
      if(code || starts!==1 || ends!==1 || !Number.isFinite(elapsed) || elapsed<=0 || lines.at(-1)!==expected[name]) reject(Error(name+' '+lines+' '+error));
      else resolve(elapsed);
    });
  });
}
function build(command, args) {
  const result=spawnSync(command,args,{encoding:'utf8'});
  if(result.status!==0) throw Error(result.error || result.stdout+result.stderr);
  return result.stdout.trim();
}
(async()=>{try {
  build('cargo',['build','--release','--bin','willow']);
  const metadata={reconstructed:true,platform:process.platform,arch:process.arch,
    cpu:os.cpus()[0].model,head:build('git',['rev-parse','HEAD']),
    dirty:build('git',['status','--porcelain','--untracked-files=no']),
    go:build('go',['version']),workers:8,
    source_sha256:Object.fromEntries(['harness.js','mutex_contention.wi','mutex.go','probes/mutex_split.wi'].map(f=>[f,crypto.createHash('sha256').update(fs.readFileSync(path.join(__dirname,f))).digest('hex')]))};
  const goBinary=path.join(dir,'mutex-go'+(process.platform==='win32'?'.exe':''));
  if(args.some(n=>n.startsWith('mutex_'))) build('go',['build','-o',goBinary,path.join(__dirname,'mutex.go')]);
  for(const name of args) {
    if(!(name in expected)) throw Error('unknown case '+name);
    const binary=path.join(dir,name+(process.platform==='win32'?'.exe':''));
    const source=name==='mutex_split' ? path.join(__dirname,'probes',name+'.wi') : path.join(__dirname,name+'.wi');
    build(process.env.WILLOW_BIN || 'target/release/willow',['build',source,'-o',binary,'--release']);
    const languages=name.startsWith('mutex_') ? ['willow','go'] : ['willow'];
    const times=Object.fromEntries(languages.map(l=>[l,[]]));
    for(let i=0;i<trials;i++) for(const language of (i%2 ? [...languages].reverse(): languages)) {
      times[language].push(await trial(language==='go'?goBinary:binary,name,language));
    }
    for(const language of languages) {
      const ms=times[language], sorted=[...ms].sort((a,b)=>a-b);
      const median=(sorted[Math.floor((trials-1)/2)]+sorted[Math.floor(trials/2)])/2;
      console.log(JSON.stringify({...metadata,case:name,language,trials,ms,median_ms:median,
        checksum:expected[name],...(name.startsWith('mutex_')?{operations:1600000,ns_per_operation:median/1.6}: {})}));
    }
  }
} finally {fs.rmSync(dir,{recursive:true,force:true});}})().catch(e=>{console.error(e);process.exitCode=1;});
