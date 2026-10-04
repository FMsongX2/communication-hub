'use strict';
const $=id=>document.getElementById(id);
const hash=new URLSearchParams(location.hash.slice(1));
if(hash.has('token')){const t=hash.get('token');if(/^[0-9a-f]{64}$/.test(t))sessionStorage.setItem('communication-hub-token',t);history.replaceState(null,'',location.pathname);}
let token=sessionStorage.getItem('communication-hub-token')||'';
let snapshot=null,records=[],scope=null,before=null,nextBefore=null,hasMore=false,refreshing=false,lastGood=null;
const providerNames={kakao:'카카오톡',discord:'Discord',slack:'Slack',notion:'Notion'};
const statusLabels={sent_verified:['전송 확인','online'],pending:['대기 중','neutral'],dispatching:['처리 중','busy'],rejected:['호출 차단','held'],held:['보류','held'],ambiguous:['처리 미확인','held'],sending_uncertain:['전송 미확인','held'],sending:['전송 미확인','held'],partial_file_held:['첨부 보류','held'],prepared_not_sent:['답변 준비','neutral'],prepared:['준비됨','neutral']};
function elem(tag,cls,text){const e=document.createElement(tag);if(cls)e.className=cls;if(text!==undefined)e.textContent=String(text);return e;}
function badge(text,cls){return elem('span','badge '+cls,text);}
function value(id,text){$(id).textContent=text;}
function time(ts){return typeof ts==='number'?new Date(ts*1000).toLocaleString('ko-KR',{month:'2-digit',day:'2-digit',hour:'2-digit',minute:'2-digit',second:'2-digit',hour12:false}):'기록 없음';}
function relative(ts){if(typeof ts!=='number')return '기록 없음';const s=Math.max(0,Date.now()/1000-ts);if(s<60)return `${Math.floor(s)}초 전`;if(s<3600)return `${Math.floor(s/60)}분 전`;return time(ts);}
async function post(path,body){const r=await fetch(path,{method:'POST',headers:{Authorization:'Bearer '+token,'Content-Type':'application/json'},body:JSON.stringify(body),cache:'no-store'});if(r.status===401){$('auth-notice').classList.remove('hidden');throw new Error('인증 연결이 필요해요. CLI에서 보드를 다시 열어주세요.');}if(!r.ok)throw new Error('변경을 저장하지 못했어요. 잠시 후 다시 시도해 주세요.');return r.json();}
async function api(path){const r=await fetch(path,{headers:{Authorization:'Bearer '+token},cache:'no-store'});if(r.status===401){$('auth-notice').classList.remove('hidden');throw new Error('인증 연결이 필요해요. CLI에서 보드를 다시 열어주세요.');}if(!r.ok)throw new Error('실행 상태를 가져오지 못했어요. 잠시 후 다시 확인해 주세요.');return r.json();}
function drawSnapshot(s){
 snapshot=s;
 const h=s.hub;
 value('hub-value',h.processing?'작업 중':h.dispatch_enabled?'온라인':'일시 중지');
 value('hub-sub',`PID ${h.pid} · 자동 발신 ${h.external_auto_send?'켜짐':'꺼짐'}`);
 const sourceLabels={online:'온라인',offline:'오프라인',disabled:'미연결',blocked:'접근 제한'};
 value('source-value',sourceLabels[s.source.status]||'미확인');value('source-sub',s.source.observed_at?`수신 확인 ${relative(s.source.observed_at)}`:'아직 수신 연결 확인 없음');
 value('backend-value',s.backend.online?'연결됨':'미연결');value('backend-sub',s.backend.checked_at?`실행기 확인 ${relative(s.backend.checked_at)}`:'세션 조회 연결 확인 중');
 value('model-label',`· 유이 ${h.model} ${h.effort}${h.service_tier ? " · "+(["priority","fast"].includes(h.service_tier)?"Fast":h.service_tier) : ""}`);
 $('adapters').replaceChildren();for(const a of s.adapters){const chip=elem('div','adapter-chip'+(a.enabled?' active':''));chip.append(elem('strong',null,providerNames[a.provider]||a.provider),elem('span',null,a.enabled?'연결 활성':a.status==='adapter_not_implemented'?'확장 예정':'미연결'));$('adapters').append(chip);}
 value('log-foot',s.body_logging?'호출 본문은 이 Mac의 비공개 DB에 보관돼요. 발신자 신원은 검증되지 않았어요.':'본문 저장은 꺼져 있어요. 태그·시각·결과만 보관돼요.');
}
let roomsData=null;
function drawRooms(d){
 roomsData=d;$('answer-unapproved').checked=d.answer_unapproved_rooms;
 const approvedCount=d.rooms.filter(r=>r.approved).length;value('session-value',String(approvedCount));value('session-sub',`승인 대기 ${d.rooms.length-approvedCount}개`);
 value('rooms-count',d.rooms.filter(r=>!r.approved).length?`대기 ${d.rooms.filter(r=>!r.approved).length}`:String(d.rooms.length));
 const tbody=$('rooms');tbody.replaceChildren();
 if(!d.rooms.length){const row=elem('tr');const cell=elem('td','empty','아직 호출된 방이 없어요. 처음 부르면 여기 승인 대기로 올라와요.');cell.colSpan=7;row.append(cell);tbody.append(row);}
 for(const r of d.rooms){
  const row=elem('tr',r.approved?'':'row-pending');
  const where=elem('td','clickable');where.title='이 방의 호출 기록 보기';where.addEventListener('click',()=>selectScope(r.key,r.title));where.append(elem('div','primary',r.title||'방 이름 없음'),elem('div','secondary',`${providerNames[r.provider]||r.provider} · ${r.account}`),elem('div','secondary mono',r.conversation_id.startsWith('name:')?'이름으로 추가됨 · 첫 호출 때 방 ID 연결':r.conversation_id));
  const state=elem('td');state.append(badge(r.approved?'승인됨':'승인 대기',r.approved?'online':'held'));
  const toggle=(field,enabled)=>{const cell=elem('td');const box=document.createElement('input');box.type='checkbox';box.checked=r[field];box.disabled=!enabled;box.setAttribute('aria-label',`${r.title} ${field==='yui'?'유이':'유미'}`);box.addEventListener('change',()=>save(r,{[field]:box.checked}));cell.append(box);return cell;};
  const verified=elem('td');verified.append(elem('div',null,r.verified_rows?`목록 ${r.verified_rows}개에서 하나뿐`:'검증 전'),elem('div','secondary',r.verified_at?relative(r.verified_at):'방 이름 검증 필요'));
  const last=elem('td');last.append(elem('div',null,relative(r.last_call_at)),elem('div','secondary',`호출 ${r.recorded_calls}건`+(r.context_reset_at?` · 맥락 초기화 ${relative(r.context_reset_at)}`:'')));
  const actions=elem('td');const box=elem('div','actions');
  const approve=elem('button','button',r.approved?'승인 해제':'승인');approve.addEventListener('click',()=>save(r,{approved:!r.approved}));
  const verify=elem('button','button','이름 검증');verify.addEventListener('click',()=>verifyRoom(r,verify));
  const reset=elem('button','button','맥락 초기화');reset.disabled=!r.approved;reset.addEventListener('click',()=>resetContext(r));
  const remove=elem('button','button','제거');remove.addEventListener('click',()=>removeRoom(r));
  box.append(approve,verify,reset,remove);actions.append(box);
  row.append(where,state,toggle('yui',true),toggle('yumi',d.yumi_configured),verified,last,actions);tbody.append(row);
 }
}
async function save(r,change){try{await post('/api/rooms',{key:r.key,approved:r.approved,yui:r.yui,yumi:r.yumi,...change});await refresh();}catch(e){value('error-notice',e.message);$('error-notice').classList.remove('hidden');}}
async function verifyRoom(r,button){button.disabled=true;button.textContent='검증 중…';try{const res=await post('/api/rooms/verify',{key:r.key});const ok=res.status==='ready';value('error-notice',ok?`“${r.title}” 방 이름이 목록 ${res.list_rows}개 중 하나뿐인 걸 확인했어요.`:`“${r.title}” 검증 보류: ${res.reason||res.status}`);$('error-notice').classList.toggle('error',!ok);$('error-notice').classList.remove('hidden');await refresh();}catch(e){value('error-notice',e.message);$('error-notice').classList.remove('hidden');}finally{button.disabled=false;button.textContent='이름 검증';}}
async function resetContext(r){if(!confirm(`“${r.title}” 방의 지난 대화를 유이·유미가 더는 참고하지 않게 할까요? 다음 호출부터 새로 시작해요.`))return;try{await post('/api/rooms/reset-context',{key:r.key});await refresh();}catch(e){value('error-notice',e.message);$('error-notice').classList.remove('hidden');}}
async function removeRoom(r){if(!confirm(`“${r.title}” 방을 목록에서 제거할까요? 이 방에서 다시 부르면 승인 대기로만 올라와요.`))return;try{await post('/api/rooms/remove',{key:r.key});await refresh();}catch(e){value('error-notice',e.message);$('error-notice').classList.remove('hidden');}}
let availableRooms=[];
function drawAvailable(){
 const q=$('room-search').value.toLocaleLowerCase();const tbody=$('available-rooms');tbody.replaceChildren();
 for(const a of availableRooms.filter(a=>!q||a.name.toLocaleLowerCase().includes(q))){
  const row=elem('tr');const name=elem('td');name.append(elem('div','primary',a.name));
  const state=elem('td');if(a.registered)state.append(badge('추가됨','online'));else if(a.duplicate)state.append(badge('이름 중복','held'));
  const act=elem('td');const add=elem('button','button','추가');add.disabled=a.registered||a.duplicate;add.addEventListener('click',()=>addRoom(a,add));act.append(add);
  row.append(name,state,act);tbody.append(row);
 }
}
async function loadAvailable(){const b=$('load-rooms');b.disabled=true;value('add-room-status','카톡 채팅 목록을 읽는 중이에요… (10초쯤 걸려요)');try{const d=await api('/api/rooms/available');if(d.status!=='ready'){value('add-room-status',`목록을 읽지 못했어요: ${d.reason||d.status}`);return;}availableRooms=d.rooms;value('add-room-status',`방 ${d.rooms.length}개 · 이름이 겹치는 방 ${d.rooms.filter(r=>r.duplicate).length}개`);drawAvailable();}catch(e){value('add-room-status',e.message);}finally{b.disabled=false;}}
async function addRoom(a,button){button.disabled=true;button.textContent='검증 중…';try{const r=await post('/api/rooms/add',{title:a.name,yui:true,yumi:!!roomsData?.yumi_configured});if(r.added){a.registered=true;value('add-room-status',`“${a.name}” 방을 추가했어요. 첫 호출 때 방 ID가 연결돼요.`);}else{value('add-room-status',`“${a.name}” 추가 보류: ${r.reason||r.status}`);}drawAvailable();await refresh();}catch(e){value('add-room-status',e.message);}finally{button.textContent='추가';button.disabled=a.registered||a.duplicate;}}
function lookup(key){return roomsData?.rooms.find(r=>r.key===key);}
function place(key){const b=lookup(key);if(b)return b;try{const p=JSON.parse(key);if(Array.isArray(p)&&p.length===3)return {provider:p[0],account:p[1],conversation_id:p[2],title:'등록되지 않은 방'};}catch{}return null;}
function drawCalls(){
 const search=$('search').value.toLocaleLowerCase();const status=$('status-filter').value;
 const filtered=records.filter(c=>{const match=status==='uncertain'?['ambiguous','sending','sending_uncertain'].includes(c.status):!status||c.status===status;return match&&(!search||`${c.body||''} ${lookup(c.conversation_key)?.title||''} ${c.event_key}`.toLocaleLowerCase().includes(search));});
 const tbody=$('calls');tbody.replaceChildren();
 if(!filtered.length){const row=elem('tr');const cell=elem('td','empty','이 범위에 기록된 호출이 없어요.');cell.colSpan=5;row.append(cell);tbody.append(row);}
 for(const c of filtered){
  const row=elem('tr','clickable');row.tabIndex=0;row.addEventListener('click',()=>details(c));row.addEventListener('keydown',e=>{if(e.key==='Enter')details(c);});
  const date=elem('td');date.append(elem('div',null,time(c.received_at)),elem('div','secondary',relative(c.received_at)));
  const b=place(c.conversation_key);const where=elem('td');where.append(elem('div','primary',b?.title||'연결 원문 없음'),elem('div','secondary',b?`${providerNames[b.provider]||b.provider} · ${b.account}`:'과거 처리 기록'));
  const trigger=elem('td');const who=c.agent==='yumi'?'유미':'유이';trigger.append(elem('span','tag',c.trigger_kind==='initial_tag'?`@[${who}]`:c.trigger_kind==='followup_tag'?`[${who}]`:'기록 없음'));
  const body=elem('td');body.append(elem('div','preview',c.body|| (c.historical_metadata_missing?'로그 추가 전 호출 · 본문 없음':'본문 저장 안 함')));
  if(c.notification_title)body.append(elem('div','secondary',`알림 제목: ${c.notification_title} · 신원 미검증`));
  const outcome=elem('td');const label=statusLabels[c.status]||[c.status,'neutral'];outcome.append(badge(...label));if(c.reason)outcome.append(elem('div','secondary',c.reason));
  row.append(date,where,trigger,body,outcome);tbody.append(row);
 }
 $('older').disabled=!nextBefore||!hasMore;
}
function details(c){
 const wrap=$('detail-content');wrap.replaceChildren();const grid=elem('div','detail-grid');const b=place(c.conversation_key);
 for(const [label,text] of [['대상',b?.title||'연결 정보 없음'],['앱 / 계정',b?`${b.provider} / ${b.account}`:'기록 없음'],['방 상태',b?(b.approved?'승인됨':'승인 대기'):'등록 안 됨'],['호출 태그',c.trigger_kind==='initial_tag'?`@[${c.agent==='yumi'?'유미':'유이'}]`:c.trigger_kind==='followup_tag'?`[${c.agent==='yumi'?'유미':'유이'}]`:'기록 없음'],['수신 시각',time(c.received_at)],['메시지 시각',time(c.occurred_at)],['메시지 ID',c.message_id||'기록 없음'],['발신자','신원 미검증'],['이벤트',c.event_key],['상태',(statusLabels[c.status]||[c.status])[0]],['오류 / 보류',c.reason||'없음']]){grid.append(elem('div','label',label),elem('div','mono',text));}
 wrap.append(grid,elem('h2',null,'호출 원문'),elem('pre',null,c.body||'원문이 저장되지 않은 기록이에요.'));
 wrap.append(elem('h2',null,'접수와 최종 전송'));
 if(!c.deliveries.length)wrap.append(elem('p','muted','전송 기록이 없어요.'));
 for(const d of c.deliveries){const row=elem('div','detail-step');const label=statusLabels[d.status]||[d.status,'neutral'];row.append(elem('span','mono',d.phase==='ack'?'접수':d.phase==='final'?'최종':d.phase),badge(...label));const text=[d.reason,d.verified_chat_name,typeof d.elapsed_seconds==='number'?`${d.elapsed_seconds.toFixed(2)}초`:null,d.attachment_sent===true?'첨부 확인':null].filter(Boolean).join(' · ');row.append(elem('span','muted',text));wrap.append(row);}
 $('details').showModal();
}
async function loadCalls(){const params=new URLSearchParams();if(scope)params.set('conversation',scope);if(before)params.set('before',before);const data=await api('/api/calls?'+params);records=data.items;nextBefore=data.next_before;hasMore=data.has_more;drawCalls();}
async function selectScope(key,title){scope=key;before=null;value('filter-label',title);$('filter-label').classList.remove('hidden');$('clear-filter').classList.remove('hidden');await refresh(false);$('calls-section').scrollIntoView({behavior:'smooth',block:'start'});}
async function refresh(full=true){
 if(refreshing)return;refreshing=true;$('refresh').disabled=true;
 try{if(full){drawSnapshot(await api('/api/snapshot'));drawRooms(await api('/api/rooms'));}await loadCalls();lastGood=Date.now();$('connection').className='badge online';value('connection','로컬 연결');value('updated','갱신 '+new Date(lastGood).toLocaleTimeString('ko-KR',{hour12:false}));$('error-notice').classList.add('hidden');$('auth-notice').classList.add('hidden');}
 catch(e){$('connection').className='badge offline';value('connection','연결 끊김');value('hub-value','연결 끊김');value('source-value','확인 불가');value('backend-value','확인 불가');value('error-notice',e.message+(lastGood?' 마지막 갱신 '+new Date(lastGood).toLocaleTimeString('ko-KR',{hour12:false}):''));$('error-notice').classList.remove('hidden');}
 finally{refreshing=false;$('refresh').disabled=false;}
}
$('refresh').addEventListener('click',()=>refresh());$('open-add-room').addEventListener('click',()=>$('add-room').showModal());$('close-add-room').addEventListener('click',()=>$('add-room').close());$('load-rooms').addEventListener('click',loadAvailable);$('room-search').addEventListener('input',drawAvailable);$('answer-unapproved').addEventListener('change',async e=>{try{await post('/api/settings',{answer_unapproved_rooms:e.target.checked});await refresh();}catch(err){value('error-notice',err.message);$('error-notice').classList.remove('hidden');}});$('search').addEventListener('input',drawCalls);$('status-filter').addEventListener('change',drawCalls);
$('clear-filter').addEventListener('click',()=>{scope=null;before=null;$('filter-label').classList.add('hidden');$('clear-filter').classList.add('hidden');refresh();});
$('older').addEventListener('click',()=>{before=nextBefore;refresh(false);});$('close-details').addEventListener('click',()=>$('details').close());
document.querySelectorAll('[data-nav]').forEach(button=>button.addEventListener('click',()=>{document.querySelectorAll('[data-nav]').forEach(b=>b.classList.toggle('selected',b===button));$(button.dataset.nav==='calls'?'calls-section':'rooms-section').scrollIntoView({behavior:'smooth',block:'start'});}));
refresh();setInterval(()=>{if(!document.hidden&&!$('details').open&&!$('add-room').open)refresh();},4000);
