// k6/ws JavaScript shim — provides ws.connect(url, params, callback)
(function() {
    var ws = {
        connect: function(url, paramsOrCallback, callbackOrUndefined) {
            var params = {};
            var callback;

            if (typeof paramsOrCallback === 'function') {
                callback = paramsOrCallback;
            } else {
                params = paramsOrCallback || {};
                callback = callbackOrUndefined;
            }

            if (typeof callback !== 'function') {
                throw new Error('ws.connect requires a callback function');
            }

            var timeout = (params && params.timeout) ? params.timeout : 60000;

            // Open the WebSocket via Rust
            var sessionId = __ws_open(url, timeout);

            var handlers = {};
            var intervals = [];
            var timeouts = [];
            var closed = false;

            var socket = {
                send: function(data) {
                    if (closed) return;
                    __ws_send(sessionId, String(data));
                },
                close: function() {
                    if (closed) return;
                    closed = true;
                    __ws_close(sessionId);
                },
                ping: function() {
                    if (closed) return;
                    __ws_ping(sessionId);
                },
                on: function(event, handler) {
                    if (!handlers[event]) handlers[event] = [];
                    handlers[event].push(handler);
                },
                setInterval: function(cb, ms) {
                    var id = intervals.length;
                    intervals.push({ cb: cb, ms: ms, next: Date.now() + ms });
                    return id;
                },
                setTimeout: function(cb, ms) {
                    var id = timeouts.length;
                    timeouts.push({ cb: cb, at: Date.now() + ms, fired: false });
                    return id;
                },
                clearInterval: function(id) {
                    if (intervals[id]) intervals[id] = null;
                },
                clearTimeout: function(id) {
                    if (timeouts[id]) timeouts[id] = null;
                },
            };

            // Call user callback to set up event handlers
            callback(socket);

            // Fire "open" handlers
            var openHandlers = handlers['open'];
            if (openHandlers) {
                for (var i = 0; i < openHandlers.length; i++) {
                    openHandlers[i]();
                }
            }

            // Event loop: receive messages and dispatch to handlers
            var recvTimeout = 100; // poll every 100ms to check intervals/timeouts
            while (!closed) {
                // Check timeouts
                var now = Date.now();
                for (var i = 0; i < timeouts.length; i++) {
                    var t = timeouts[i];
                    if (t && !t.fired && now >= t.at) {
                        t.fired = true;
                        t.cb();
                    }
                }

                // Check intervals
                for (var i = 0; i < intervals.length; i++) {
                    var iv = intervals[i];
                    if (iv && now >= iv.next) {
                        iv.next = now + iv.ms;
                        iv.cb();
                    }
                }

                if (closed) break;

                // Receive next event from Rust (blocks up to recvTimeout ms)
                var event = __ws_recv(sessionId, recvTimeout);

                if (event.type === 'message') {
                    var msgHandlers = handlers['message'];
                    if (msgHandlers) {
                        for (var i = 0; i < msgHandlers.length; i++) {
                            msgHandlers[i](event.data);
                        }
                    }
                } else if (event.type === 'binaryMessage') {
                    var binHandlers = handlers['binaryMessage'];
                    if (binHandlers) {
                        for (var i = 0; i < binHandlers.length; i++) {
                            binHandlers[i](event.data);
                        }
                    }
                } else if (event.type === 'ping') {
                    var pingHandlers = handlers['ping'];
                    if (pingHandlers) {
                        for (var i = 0; i < pingHandlers.length; i++) {
                            pingHandlers[i]();
                        }
                    }
                } else if (event.type === 'pong') {
                    var pongHandlers = handlers['pong'];
                    if (pongHandlers) {
                        for (var i = 0; i < pongHandlers.length; i++) {
                            pongHandlers[i]();
                        }
                    }
                } else if (event.type === 'error') {
                    var errHandlers = handlers['error'];
                    if (errHandlers) {
                        for (var i = 0; i < errHandlers.length; i++) {
                            errHandlers[i]({ error: event.data });
                        }
                    }
                    closed = true;
                } else if (event.type === 'close') {
                    closed = true;
                }
                // 'timeout' means no event in the poll window — loop continues
            }

            // Fire "close" handlers
            var closeHandlers = handlers['close'];
            if (closeHandlers) {
                for (var i = 0; i < closeHandlers.length; i++) {
                    closeHandlers[i]();
                }
            }

            // Cleanup
            __ws_cleanup(sessionId);
        }
    };

    globalThis.ws = ws;
})();
