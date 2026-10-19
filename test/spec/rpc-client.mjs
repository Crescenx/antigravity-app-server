import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
export class RpcClient {
  constructor(command,args,options={}) {
    this.child=spawn(command,args,{...options,stdio:['pipe','pipe','pipe']});
    this.serial=0; this.pending=new Map(); this.events=[]; this.stderr='';
    this.child.stdin.on('error',()=>{});
    this.child.stderr.on('data',b=>{this.stderr=(this.stderr+b).slice(-8000);});
    createInterface({input:this.child.stdout}).on('line',line=>{
      let m;try{m=JSON.parse(line);}catch{return this.fail(Error('invalid JSON output'));}
      if(m.method)this.events.push(m);
      else if(this.pending.has(m.id)){const p=this.pending.get(m.id);clearTimeout(p.timer);this.pending.delete(m.id);p.resolve(m);}
    });
    this.child.on('error',e=>this.fail(e));
    this.child.on('exit',()=>this.fail(Error('RPC child exited')));
  }
  fail(error){for(const p of this.pending.values()){clearTimeout(p.timer);p.reject(error);}this.pending.clear();}
  call(method,params={},envelope=true) {
    const id=++this.serial;
    return new Promise((resolve,reject)=>{
      const timer=setTimeout(()=>{this.pending.delete(id);reject(Error(`${method} timed out`));},12000);
      this.pending.set(id,{resolve,reject,timer});
      this.child.stdin.write(JSON.stringify({...(envelope?{jsonrpc:'2.0'}:{}),id,method,params})+'\n');
    });
  }
  async close(){
    if(this.child.exitCode!==null || this.child.signalCode!==null)return;
    const exited=once(this.child,'exit');
    this.child.stdin.end();
    const timer=setTimeout(()=>this.child.kill('SIGKILL'),8000);
    try{await exited;}finally{clearTimeout(timer);this.fail(Error('client closed'));}
  }
}
