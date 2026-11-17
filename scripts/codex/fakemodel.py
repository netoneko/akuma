import json,sys
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
PORT=int(sys.argv[1]); LOG=open(sys.argv[2],'a',buffering=1)
CMD=sys.argv[3] if len(sys.argv)>3 else 'ls /tmp | head -3'
def sse(evs): return ''.join('event: %s\ndata: %s\n\n'%(e['type'],json.dumps(e)) for e in evs).encode()
class H(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.0'
    def log_message(self,*a): pass
    def do_GET(self):
        LOG.write('GET %s\n'%self.path); b=b'{"models":[]}'
        self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_POST(self):
        n=int(self.headers.get('Content-Length') or 0); body=self.rfile.read(n)
        try: j=json.loads(body)
        except Exception: j={}
        items=j.get('input',[]) if isinstance(j,dict) else []
        outs=[i for i in items if isinstance(i,dict) and 'output' in i]
        LOG.write('POST %s items=%d tool_outputs=%d\n'%(self.path,len(items),len(outs)))
        if outs:
            LOG.write('TOOL_OUTPUT %s\n'%json.dumps(outs[-1].get('output'))[:1500])
            evs=[{'type':'response.created','response':{'id':'r2','status':'in_progress'}},
                 {'type':'response.output_item.done','output_index':0,'item':{'type':'message','role':'assistant','id':'m1','content':[{'type':'output_text','text':'done'}]}},
                 {'type':'response.completed','response':{'id':'r2','status':'completed','usage':{'input_tokens':1,'output_tokens':1,'total_tokens':2}}}]
        else:
            code='const r = await tools.exec_command({cmd:%s}); text(JSON.stringify(r));'%json.dumps(CMD)
            evs=[{'type':'response.created','response':{'id':'r1','status':'in_progress'}},
                 {'type':'response.output_item.done','output_index':0,'item':{'type':'custom_tool_call','id':'ctc1','status':'completed','call_id':'call1','name':'exec','input':code}},
                 {'type':'response.completed','response':{'id':'r1','status':'completed','usage':{'input_tokens':1,'output_tokens':1,'total_tokens':2}}}]
        d=sse(evs); self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Content-Length',str(len(d))); self.end_headers(); self.wfile.write(d)
ThreadingHTTPServer(('0.0.0.0',PORT),H).serve_forever()
