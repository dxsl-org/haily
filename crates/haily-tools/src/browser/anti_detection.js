(function() {
    // Detect platform from UA to avoid hardcoded OS mismatch on Mac/Linux
    var ua = navigator.userAgent;
    var plat = ua.indexOf('Win') !== -1 ? 'Windows' : ua.indexOf('Mac') !== -1 ? 'macOS' : 'Linux';
    var arch = plat === 'Windows' ? 'x86' : 'arm64';
    var platVer = plat === 'Windows' ? '10.0.0' : plat === 'macOS' ? '14.0.0' : '6.1.0';

    // Override navigator.webdriver
    try { Object.defineProperty(navigator, 'webdriver', { get: function() { return undefined; }, configurable: true }); } catch (_) {}

    // Override navigator.userAgentData
    var m = ua.match(/Chrome\/([\d.]+)/);
    var cv = m ? m[1] : '130.0.0.0';
    var maj = cv.split('.')[0];
    var uad = {
        brands: [{ brand:'Chromium', version:maj },{ brand:'Google Chrome', version:maj },{ brand:'Not-A.Brand', version:'99' }],
        mobile: false, platform: plat,
        getHighEntropyValues: function(hints) {
            return Promise.resolve({ brands:this.brands, mobile:false, platform:plat,
                platformVersion:platVer, architecture:arch, bitness:'64', model:'',
                uaFullVersion:cv, wow64:false,
                fullVersionList:[{ brand:'Chromium',version:cv },{ brand:'Google Chrome',version:cv },{ brand:'Not-A.Brand',version:'99.0.0.0' }] });
        },
        toJSON: function() { return { brands:this.brands, mobile:this.mobile, platform:this.platform }; },
    };
    try { Object.defineProperty(navigator, 'userAgentData', { get: function() { return uad; }, configurable: true }); } catch (_) {}

    // Override navigator.languages — fingerprint signal checked by many antibot systems
    try { Object.defineProperty(navigator, 'languages', { get: function() { return ['en-US', 'en']; }, configurable: true }); } catch (_) {}

    // Override navigator.plugins — 5-entry list with full MIME types matching real Chrome
    try {
        Object.defineProperty(navigator, 'plugins', { configurable: true, get: function() { return ({
            0:{ name:'PDF Viewer', filename:'internal-pdf-viewer', description:'Portable Document Format', length:1,
                0:{ type:'application/pdf', suffixes:'pdf', description:'Portable Document Format' } },
            1:{ name:'Chrome PDF Plugin', filename:'internal-pdf-viewer', description:'', length:1,
                0:{ type:'application/x-google-chrome-pdf', suffixes:'pdf', description:'' } },
            2:{ name:'Chrome PDF Viewer', filename:'mhjfbmdgcfjbbpaeojofohoefgiehjai', description:'', length:1,
                0:{ type:'application/pdf', suffixes:'pdf', description:'' } },
            3:{ name:'Native Client', filename:'internal-nacl-plugin', description:'', length:2,
                0:{ type:'application/x-nacl', suffixes:'', description:'Native Client Executable' },
                1:{ type:'application/x-pnacl', suffixes:'', description:'Portable Native Client Executable' } },
            4:{ name:'Chromium PDF Plugin', filename:'internal-pdf-viewer', description:'Portable Document Format', length:1,
                0:{ type:'application/x-google-chrome-pdf', suffixes:'pdf', description:'Portable Document Format' } },
            length:5,
            item: function(i) { return this[i]||null; },
            namedItem: function(n) { for(var i=0;i<this.length;i++) if(this[i]&&this[i].name===n) return this[i]; return null; },
            refresh: function() {},
            [Symbol.iterator]: function*(){ for(var i=0;i<this.length;i++) if(this[i]) yield this[i]; },
        }); } });
    } catch (_) {}

    // Override window.chrome
    try {
        if (!window.chrome) window.chrome = {};
        if (!window.chrome.runtime) window.chrome.runtime = {
            connect: function(){ return { onMessage:{addListener:function(){}}, postMessage:function(){}, onDisconnect:{addListener:function(){}} }; },
            sendMessage: function(){}, id:undefined,
        };
        if (!window.chrome.csi) window.chrome.csi = function() { return { onloadT:Date.now(), pageT:performance.now(), startE:Date.now(), tran:15 }; };
        if (!window.chrome.loadTimes) window.chrome.loadTimes = function() { return ({
            commitLoadTime:Date.now()/1000, connectionInfo:'h2', finishDocumentLoadTime:Date.now()/1000,
            finishLoadTime:Date.now()/1000, firstPaintAfterLoadTime:0, firstPaintTime:Date.now()/1000,
            navigationType:'Other', npnNegotiatedProtocol:'h2', requestTime:Date.now()/1000,
            startLoadTime:Date.now()/1000, wasAlternateProtocolAvailable:false, wasFetchedViaSpdy:true, wasNpnNegotiated:true,
        }); };
    } catch (_) {}

    // Override navigator.permissions
    try {
        var orig = navigator.permissions && navigator.permissions.query && navigator.permissions.query.bind(navigator.permissions);
        if (orig) navigator.permissions.query = function(d) {
            return d.name === 'notifications' ? Promise.resolve({ state:Notification.permission, onchange:null }) : orig(d);
        };
    } catch (_) {}

    // Override screen dimensions — 1280×800 launch window looks like automation VM;
    // real users have 1920×1080+ screens with taskbar space
    try {
        var _sd = { width:1920, availWidth:1920, height:1080, availHeight:1040, colorDepth:24, pixelDepth:24 };
        Object.keys(_sd).forEach(function(k) {
            try { Object.defineProperty(screen, k, { get: function(v) { return function() { return v; }; }(_sd[k]), configurable:true }); } catch(_) {}
        });
    } catch (_) {}

    // WebGL unmasked vendor/renderer — platform-matched strings to avoid GPU/OS mismatch
    // 37445=UNMASKED_VENDOR_WEBGL, 37446=UNMASKED_RENDERER_WEBGL (WEBGL_debug_renderer_info)
    try {
        var _wglV = 'Intel Inc.';
        var _wglR = plat === 'macOS'
            ? 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)'
            : plat === 'Linux'
                ? 'ANGLE (Intel, Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL 4.6)'
                : 'ANGLE (Intel, Intel(R) UHD Graphics 620 Direct3D11 vs_5_0 ps_5_0, D3D11)';
        var _pgp = function(o) { return function(p) {
            if (p === 37445) return _wglV;
            if (p === 37446) return _wglR;
            return o.call(this, p);
        }; };
        WebGLRenderingContext.prototype.getParameter = _pgp(WebGLRenderingContext.prototype.getParameter);
        if (typeof WebGL2RenderingContext !== 'undefined') {
            WebGL2RenderingContext.prototype.getParameter = _pgp(WebGL2RenderingContext.prototype.getParameter);
        }
    } catch (_) {}

    // Canvas fingerprint noise — modify top-left pixel on toDataURL/toBlob then restore.
    // Session-seeded so fingerprint differs per session; visual output is unchanged.
    try {
        var _cSeed = Math.random() * 0xFFFF | 0;
        var _cnoise = function(c) {
            var ctx2 = c.getContext && c.getContext('2d');
            if (!ctx2 || c.width < 1 || c.height < 1) return null;
            var s = ctx2.getImageData(0, 0, 1, 1);
            var n = ctx2.createImageData(1, 1);
            n.data[0] = (s.data[0] + (_cSeed & 1)) & 0xFF;
            n.data[1] = s.data[1]; n.data[2] = s.data[2]; n.data[3] = s.data[3];
            ctx2.putImageData(n, 0, 0);
            return s;
        };
        var _otdu = HTMLCanvasElement.prototype.toDataURL;
        HTMLCanvasElement.prototype.toDataURL = function() {
            var ctx2 = this.getContext && this.getContext('2d');
            var sv = _cnoise(this);
            var r = _otdu.apply(this, arguments);
            if (sv && ctx2) ctx2.putImageData(sv, 0, 0);
            return r;
        };
        var _otb = HTMLCanvasElement.prototype.toBlob;
        HTMLCanvasElement.prototype.toBlob = function(cb) {
            var ctx2 = this.getContext && this.getContext('2d');
            var sv = _cnoise(this);
            _otb.call(this, function(blob) { if (sv && ctx2) ctx2.putImageData(sv, 0, 0); cb(blob); });
        };
    } catch (_) {}

    // AudioContext fingerprint — add imperceptible sub-epsilon offset to first sample
    try {
        var _ogcd = AudioBuffer.prototype.getChannelData;
        AudioBuffer.prototype.getChannelData = function() {
            var d = _ogcd.apply(this, arguments);
            if (d.length > 0) d[0] += 1e-7;
            return d;
        };
    } catch (_) {}
})();
