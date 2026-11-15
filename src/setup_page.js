'use strict';
const $ = selector => document.querySelector(selector);
const token = new URLSearchParams(location.hash.slice(1)).get('token') || sessionStorage.getItem('shareboxSetupToken') || '';
if (token) sessionStorage.setItem('shareboxSetupToken', token);
history.replaceState(null, '', location.pathname);
let state = null;
let busy = false;
const escapeHTML = value => String(value ?? '').replace(/[&<>"']/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[char]));
async function api(path, body) {
  const response = await fetch('/api/setup/' + path, {method: body === undefined ? 'GET' : 'POST', headers:{Authorization:'Bearer ' + token, 'Content-Type':'application/json'}, ...(body === undefined ? {} : {body:JSON.stringify(body)})});
  const value = await response.json();
  if (!response.ok) throw new Error(value.error || 'Setup request failed');
  return value;
}
function message(text, error = false) { $('#message').textContent=text; $('#message').className='alert ' + (error ? 'warning' : 'success'); }
function modeFields() {
  const docker=$('#mode').value==='docker';
  $('#docker-fields').classList.toggle('hidden',!docker); $('#native-warning').classList.toggle('hidden',$('#mode').value!=='native');
  $('#image').required=docker; if(!docker) $('#network').checked=false;
}
function render(value, initial) {
  state=value;
  $('#registration-status').textContent=value.paired ? `${value.name} · Registered` : 'Not registered';
  $('#service-status').textContent=value.service.error || (value.service.running ? 'Running' : value.service.processRunning ? 'Running in a terminal' : value.service.conflict ? 'Another worker uses this account’s service' : value.service.registered ? 'Stopped' : 'Not installed');
  const docker=value.capabilities.docker;
  $('#docker-status').textContent=docker.message;
  $('#mode option[value="docker"]').disabled=!docker.ready;
  $('#master-status').textContent=value.master.message + (value.master.version ? ` · Master ${value.master.version}` : '');
  if(value.master.worker?.runningJobId) $('#master-status').textContent += ' · Running job ' + value.master.worker.runningJobId;
  $('#config-path').textContent='Configuration: '+value.configPath;
  $('#replace-pairing-label').classList.toggle('hidden',!value.paired);
  const replacing=value.paired && $('#replace-pairing').checked;
  $('#registration-fields').classList.toggle('hidden',value.paired&&!replacing);
  $('#registration-link').required=!value.paired||replacing; $('#worker-name').required=!value.paired||replacing;
  $('#install-service').classList.toggle('hidden',!value.paired || value.service.registered || !!value.service.error || value.service.conflict || value.service.processRunning);
  $('#start-service').classList.toggle('hidden',!value.paired || !value.service.registered || value.service.running || !!value.service.error || value.service.processRunning);
  $('#stop-service').classList.toggle('hidden',!value.service.running || !!value.service.error);
  $('#save').textContent=replacing ? 'Replace pairing & save task' : value.paired ? 'Save snapshot task' : 'Register worker & save task';
  $('#save').disabled=!!value.service.running || !!value.service.processRunning || !!value.service.error || busy || ($('#mode').value==='docker'&&!docker.ready);
  $('#save-help').textContent=value.service.running ? 'Finish active tests, then Stop worker above before changing its task. Start it again after saving.' : value.paired ? 'After saving, start the worker to advertise the task to the master.' : 'Registration saves your pairing first. Then choose Install & start service above.';
  $('#tasks').innerHTML=value.tasks.length ? value.tasks.map(task=>`<div class="setup-task"><strong>${escapeHTML(task.name)}</strong><span>${task.snapshot ? (task.execution==='docker' ? 'Docker sandbox · uploaded snapshot' : 'Native · uploaded snapshot') : 'Native · local checkout'}</span></div>`).join('') : '<p class="sub">No tasks configured.</p>';
  if(initial) {
    $('#workspace').value=value.managedTask?.workspace || value.defaultWorkspace;
    $('#worker-name').value=value.name || value.defaultName || 'My worker';
    $('#mode').value=value.capabilities.docker.ready ? 'docker' : ''; 
    if(value.managedTask) {
      $('#preset').value=value.managedTask.preset || 'node-test';
      $('#mode').value=value.managedTask.snapshot?.mode || 'docker';
      $('#image').value=value.managedTask.snapshot?.image || 'node:22-bookworm-slim';
      $('#network').checked=value.managedTask.snapshot?.network==='bridge';
      $('#dependencies').checked=!!value.managedTask.snapshot?.prepare?.length;
    }
  }
  modeFields();
}
async function refresh(initial=false) { render(await api('status'), initial); }
async function action(fn) {
  if(busy)return;busy=true;
  document.querySelectorAll('button').forEach(button=>button.disabled=true);
  try { await fn(); await refresh(); } catch(error){ message(error.message,true); } finally {busy=false;document.querySelectorAll('button').forEach(button=>button.disabled=false);if(state)render(state,false);}
}
$('#mode').addEventListener('change',()=>{modeFields();if(state)render(state,false);});
$('#replace-pairing').addEventListener('change',()=>{if(state)render(state,false);});
let closed=false;
$('#close-setup').addEventListener('click',async()=>{try{await api('close',{});closed=true;document.querySelectorAll('button,input,select').forEach(control=>control.disabled=true);message('Setup closed. Your worker service keeps its current state. You can close this tab.');}catch(error){message(error.message,true);}});
$('#refresh').addEventListener('click',()=>action(()=>refresh()));
$('#check-docker').addEventListener('click',()=>action(async()=>{const result=await api('check-docker',{});message(result.docker.message,!result.docker.ready);}));
$('#pull-image').addEventListener('click',()=>action(async()=>{message('Downloading the selected Docker image. This can take a few minutes…');const result=await api('pull-image',{image:$('#image').value.trim()});message('Downloaded '+result.image+'. You can now save the Docker task.');}));
for (const [id, actionName] of [['install-service','install'],['start-service','start'],['stop-service','stop']]) {
  $('#'+id).addEventListener('click',()=>action(async()=>{if(actionName==='stop' && !confirm('Stop this worker? Finish or cancel active tests first.'))return;await api('service',{action:actionName});message(actionName==='stop'?'Worker stopped.':'Worker service started.');}));
}
$('#setup-form').addEventListener('submit',event=>{event.preventDefault();action(async()=>{
  const settings={preset:$('#preset').value,mode:$('#mode').value,image:$('#image').value.trim(),network:$('#network').checked?'bridge':'none',workspace:$('#workspace').value.trim(),installDependencies:$('#dependencies').checked,allowSnapshots:$('#consent').checked};
  const pairing=!state?.paired || $('#replace-pairing').checked;
  const response=await api(pairing ? 'pair':'task',pairing?{...settings,replacePairing:$('#replace-pairing').checked,link:$('#registration-link').value.trim(),name:$('#worker-name').value.trim()}:settings);
  $('#replace-pairing').checked=false;
  $('#registration-link').value='';$('#consent').checked=false;message(response.message);await refresh(true);
});});
refresh(true).catch(error=>message(error.message,true));
setInterval(()=>{if(!busy&&!closed)refresh().catch(error=>message(error.message,true));},15000);
