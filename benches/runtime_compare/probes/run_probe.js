// Reconstructed phase timer; process startup is excluded.
const {spawn} = require('node:child_process');
const child = spawn(process.argv[2], [], {env:{...process.env,WILLOW_WORKERS:process.argv[3] || '8'}});
let buffer = '', start, elapsed, checksum;
const timer = setTimeout(() => { child.kill('SIGKILL'); }, 30000);
child.stdout.on('data', data => {
  buffer += data;
  let pos;
  while ((pos = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0,pos).trim(); buffer = buffer.slice(pos+1);
    const now = process.hrtime.bigint();
    if(line === '@start scalar') start = now;
    if(line === '@end scalar') {
      elapsed = Number(now-start)/4e6;
      console.log(elapsed, 'ns/iteration');
    }
    if (/^[0-9]+$/.test(line)) checksum = line;
    console.log(line);
  }
});
child.stderr.pipe(process.stderr);
child.on('error', error => {clearTimeout(timer); console.error(error); process.exitCode=1;});
child.on('close', code => {
  clearTimeout(timer);
  process.exitCode = code === 0 && Number.isFinite(elapsed) && checksum === '7999998000000' ? 0 : 1;
});
