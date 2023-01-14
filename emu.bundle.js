(()=>{"use strict";var e={},t={};function i(o){var r=t[o];if(void 0!==r)return r.exports;var n=t[o]={exports:{}};return e[o](n,n.exports,i),n.exports}i.m=e,i.d=(e,t)=>{for(var o in t)i.o(t,o)&&!i.o(e,o)&&Object.defineProperty(e,o,{enumerable:!0,get:t[o]})},i.f={},i.e=e=>Promise.all(Object.keys(i.f).reduce(((t,o)=>(i.f[o](e,t),t)),[])),i.u=e=>e+".bundle.js",i.miniCssF=e=>{},i.g=function(){if("object"==typeof globalThis)return globalThis;try{return this||new Function("return this")()}catch(e){if("object"==typeof window)return window}}(),i.o=(e,t)=>Object.prototype.hasOwnProperty.call(e,t),i.r=e=>{"undefined"!=typeof Symbol&&Symbol.toStringTag&&Object.defineProperty(e,Symbol.toStringTag,{value:"Module"}),Object.defineProperty(e,"__esModule",{value:!0})},(()=>{var e;i.g.importScripts&&(e=i.g.location+"");var t=i.g.document;if(!e&&t&&(t.currentScript&&(e=t.currentScript.src),!e)){var o=t.getElementsByTagName("script");o.length&&(e=o[o.length-1].src)}if(!e)throw new Error("Automatic publicPath is not supported in this browser");e=e.replace(/#.*$/,"").replace(/\?.*$/,"").replace(/\/[^\/]+$/,"/"),i.p=e})(),(()=>{i.b=self.location+"";var e={306:1};i.f.i=(t,o)=>{e[t]||importScripts(i.p+i.u(t))};var t=self.webpackChunkdust_web=self.webpackChunkdust_web||[],o=t.push.bind(t);t.push=t=>{var[r,n,a]=t;for(var s in n)i.o(n,s)&&(i.m[s]=n[s]);for(a&&a(i);r.length;)e[r.pop()]=1;o(t)}})();var o,r,n,a;function s(e,t){postMessage(e,t)}class u{constructor(e,t){this.callback=t,this.handleTimeoutCallback=this.handleTimeout.bind(this),this.limit=e}get limit(){return this.limit_}set limit(e){e!==this.limit_&&(this.limit_=e,clearTimeout(this.timeoutId),this.timeout=null===e?0:1e3/e,this.expectedTimeoutTime=e?(this.expectedTimeoutTime||performance.now())+this.timeout:0,this.timeoutId=setTimeout(this.handleTimeoutCallback,Math.max(0,this.expectedTimeoutTime-performance.now())))}handleTimeout(){if(this.callback(),this.timeout){const e=performance.now();this.expectedTimeoutTime=Math.max(this.expectedTimeoutTime+this.timeout,e),this.timeoutId=setTimeout(this.handleTimeoutCallback,this.expectedTimeoutTime-e)}else setTimeout(this.handleTimeoutCallback,0)}}o=void 0,r=void 0,a=function*(){const e=yield i.e(235).then(i.bind(i,235));yield e.default();let t,o=!1,r=new u(60,(function(){if(!o)return;const e=t.run_frame();s({type:3,buffer:e},[e.buffer]);const i=performance.now();if(i-n>=1e3){n=i;const e=t.export_save();s({type:2,buffer:e,triggerDownload:!1},[e.buffer])}})),n=performance.now();self.onmessage=i=>{var a,u;const c=i.data;switch(c.type){case 0:t=e.create_emu_state(c.bios7,c.bios9,c.firmware,c.rom,void 0,c.saveType,c.hasIR,e.WbgModel.Lite,((e,t)=>{s({type:5,l:e,r:t},[e.buffer,t.buffer])})),s({type:1,module:e.internal_get_module(),memory:e.internal_get_memory()});break;case 1:t.reset();break;case 2:{const e=t.export_save();t.free(),s({type:4,buffer:e},[e.buffer]),close();break}case 3:t.load_save(new Uint8Array(c.buffer));break;case 4:{const e=t.export_save();n=performance.now(),s({type:2,buffer:e,triggerDownload:!0},[e.buffer]);break}case 5:t.update_input(c.pressed,c.released),void 0!==c.touchPos&&t.update_touch(null===(a=c.touchPos)||void 0===a?void 0:a[0],null===(u=c.touchPos)||void 0===u?void 0:u[1]);break;case 6:o=c.value;break;case 7:r.limit=c.value?60:null}},s({type:0})},new((n=void 0)||(n=Promise))((function(e,t){function i(e){try{u(a.next(e))}catch(e){t(e)}}function s(e){try{u(a.throw(e))}catch(e){t(e)}}function u(t){var o;t.done?e(t.value):(o=t.value,o instanceof n?o:new n((function(e){e(o)}))).then(i,s)}u((a=a.apply(o,r||[])).next())}))})();