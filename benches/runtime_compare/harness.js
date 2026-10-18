// Reconstructed Willow-only acceptance harness. Phase timing excludes process startup.
const {spawn, spawnSync} = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const args = process.argv.slice(2);
if(args.shift() !== 'run') throw Error('usage: harness.js run CASE... --trials N');
let trials = 5; const ti = args.indexOf('--trials');
if(ti >= 0) { trials = Number(args[ti+1]); args.splice(ti,2); }
const expected = {mutex_contention:'400000',atomic_counter:'400000',producer_consumer:'19999900000',gc_scheduler:'10000600000'};
const dir = fs.mkdtempSync(path.join(os.tmpdir(),'willow-8hq4-41-'));
async function trial(binary, name) {
  return new Promise((resolve,reject) => {
    const child = spawn(binary, [], {env:{...process.env,WILLOW_WORKERS:'8'}});
    let buffer='', lines=[], start, elapsed, error='';
    const timer=setTimeout(()=>{child.kill('SIGKILL'); reject(Error('timeout '+name));},120000);
    child.stderr.on('data',d=>error+=d);
    child.stdout.on('data',d=> {buffer+=d; let pos;
      while((pos=buffer.indexOf('\n'))>=0) {
        const line=buffer.slice(0,pos).trim(); buffer=buffer.slice(pos+1); lines.push(line);
        const now=process.hrtime.bigint();
        if(line==='@start work') start=now;
        if(line==='@end work') elapsed=Number(now-start)/1e6;
      }
    });
    child.on('error',reject);
    child.on('close',code=>{clearTimeout(timer);
      if(code || !Number.isFinite(elapsed) || lines.at(-1)!==expected[name]) reject(Error(name+' '+lines+' '+error));
      else resolve(elapsed);
    });
  });
}
(async()=>{try {
  for(const name of args) {
    if(!(name in expected)) throw Error('unknown case '+name);
    const binary=path.join(dir,name+(process.platform==='win32'?'.exe':''));
    const build=spawnSync(process.env.WILLOW_BIN || 'target/release/willow',['build',path.join(__dirname,name+'.wi'),'-o',binary,'--release'],{encoding:'utf8'});
    if(build.status!==0) throw Error(build.stdout+build.stderr);
    const ms=[]; for(let i=0;i<trials;i++) ms.push(await trial(binary,name));
    console.log(JSON.stringify({case:name,trials,workers:8,ms,median_ms:[...ms].sort((a,b)=>a-b)[Math.floor(trials/2)],reconstructed:true,platform:process.platform,arch:process.arch}));
  }
} finally {fs.rmSync(dir,{recursive:true,force:true});}})().catch(e=>{console.error(e);process.exitCode=1;});
