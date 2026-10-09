import {test} from 'node:test';
import assert from 'node:assert/strict';
import {AllumetteBrowserClient,AllumetteError} from './index.js';
test('configuration rejects credentials and navigation parameters',()=>{
 for(const url of ['ftp://api.test','https://user:password@api.test','https://api.test?token=x','https://api.test/path'])assert.throws(()=>new AllumetteBrowserClient(url));
});
test('guest login signs the actual server challenge and keeps credentials in memory',async()=>{
 const challenge='server-challenge';let payload;
 const c=new AllumetteBrowserClient('https://api.test',{fetch:async(url,opts)=>{
  if(url.endsWith('/challenge'))return Response.json({challenge});
  payload=JSON.parse(opts.body);return Response.json({token:'identity-token'});
 }});
 const user=await c.loginGuest(' Guest ');
 const key=await crypto.subtle.importKey('raw',Buffer.from(payload.public_key_b64,'base64'),'Ed25519',true,['verify']);
 assert.equal(await crypto.subtle.verify('Ed25519',key,Buffer.from(payload.signature_b64,'base64'),new TextEncoder().encode(challenge)),true);
 assert.equal(user.username,'Guest');assert.equal(c.token,'identity-token');c.logout();assert.equal(c.token,null);
});
test('session context preserves every STUN/TURN credential and resolves WSS',async()=>{
 const ice=[{urls:['stun:test']},{urls:['turn:test'],username:'temporary',credential:'credential'}];
 const c=new AllumetteBrowserClient('https://api.test',{fetch:async(url)=>Response.json(url.endsWith('/ice-servers')?ice:{schema_version:1,signaling_path:'ticket',session_id:'session'})});
 const session=await c.getSessionContext('lobby');assert.deepEqual(session.ice_servers,ice);assert.equal(session.signaling_url,'wss://api.test/ticket');
});
test('401 clears identity and 409 incompatibility remains explicit',async()=>{
 let status=409;const c=new AllumetteBrowserClient('https://api.test',{fetch:async()=>Response.json({code:'incompatible_build'},{status})});c.token='identity';
 await assert.rejects(c.listLobbies(),e=>e instanceof AllumetteError&&e.status===409&&e.code==='incompatible_build');assert.equal(c.token,'identity');
 status=401;await assert.rejects(c.listLobbies());assert.equal(c.token,null);
});
