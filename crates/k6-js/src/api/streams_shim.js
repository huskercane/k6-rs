(function() {
    // ReadableStreamDefaultController
    function ReadableStreamDefaultController(stream) {
        this._stream = stream;
        this._closeRequested = false;
    }

    ReadableStreamDefaultController.prototype.enqueue = function(chunk) {
        if (this._closeRequested) throw new TypeError('Cannot enqueue after close');
        this._stream._queue.push(chunk);
    };

    ReadableStreamDefaultController.prototype.close = function() {
        this._closeRequested = true;
        this._stream._closed = true;
    };

    ReadableStreamDefaultController.prototype.error = function(e) {
        this._stream._errored = true;
        this._stream._error = e;
    };

    Object.defineProperty(ReadableStreamDefaultController.prototype, 'desiredSize', {
        get: function() {
            return this._stream._highWaterMark - this._stream._queue.length;
        }
    });

    // ReadableStreamDefaultReader
    function ReadableStreamDefaultReader(stream) {
        this._stream = stream;
        this._closed = false;
    }

    ReadableStreamDefaultReader.prototype.read = function() {
        var stream = this._stream;
        if (stream._errored) {
            throw stream._error;
        }
        if (stream._queue.length > 0) {
            return { value: stream._queue.shift(), done: false };
        }
        if (stream._closed) {
            return { value: undefined, done: true };
        }
        // Pull if available
        if (stream._pullFn) {
            stream._pullFn(stream._controller);
            if (stream._queue.length > 0) {
                return { value: stream._queue.shift(), done: false };
            }
        }
        return { value: undefined, done: stream._closed };
    };

    ReadableStreamDefaultReader.prototype.cancel = function() {
        this._stream._closed = true;
        this._stream._queue = [];
        if (this._stream._cancelFn) {
            this._stream._cancelFn();
        }
    };

    ReadableStreamDefaultReader.prototype.releaseLock = function() {
        this._stream._reader = null;
    };

    // ReadableStream
    globalThis.ReadableStream = function(underlyingSource, strategy) {
        this._queue = [];
        this._closed = false;
        this._errored = false;
        this._error = null;
        this._reader = null;
        this._highWaterMark = (strategy && strategy.highWaterMark) || 1;
        this._controller = new ReadableStreamDefaultController(this);
        this._pullFn = underlyingSource && underlyingSource.pull ? underlyingSource.pull.bind(underlyingSource) : null;
        this._cancelFn = underlyingSource && underlyingSource.cancel ? underlyingSource.cancel.bind(underlyingSource) : null;

        if (underlyingSource && underlyingSource.start) {
            underlyingSource.start(this._controller);
        }
    };

    ReadableStream.prototype.getReader = function() {
        if (this._reader) throw new TypeError('ReadableStream already has a reader');
        var reader = new ReadableStreamDefaultReader(this);
        this._reader = reader;
        return reader;
    };

    ReadableStream.prototype.cancel = function() {
        this._closed = true;
        this._queue = [];
        if (this._cancelFn) this._cancelFn();
    };

    ReadableStream.prototype.pipeThrough = function(transformStream) {
        var reader = this.getReader();
        var writable = transformStream.writable;
        var writer = writable.getWriter();

        var result;
        while (true) {
            result = reader.read();
            if (result.done) break;
            writer.write(result.value);
        }
        writer.close();
        reader.releaseLock();

        return transformStream.readable;
    };

    ReadableStream.prototype.pipeTo = function(writableStream) {
        var reader = this.getReader();
        var writer = writableStream.getWriter();

        var result;
        while (true) {
            result = reader.read();
            if (result.done) break;
            writer.write(result.value);
        }
        writer.close();
        reader.releaseLock();
    };

    Object.defineProperty(ReadableStream.prototype, 'locked', {
        get: function() { return this._reader !== null; }
    });

    // WritableStreamDefaultWriter
    function WritableStreamDefaultWriter(stream) {
        this._stream = stream;
    }

    WritableStreamDefaultWriter.prototype.write = function(chunk) {
        if (this._stream._closed) throw new TypeError('Cannot write to a closed stream');
        if (this._stream._writeFn) {
            this._stream._writeFn(chunk);
        }
        this._stream._written.push(chunk);
    };

    WritableStreamDefaultWriter.prototype.close = function() {
        this._stream._closed = true;
        if (this._stream._closeFn) {
            this._stream._closeFn();
        }
    };

    WritableStreamDefaultWriter.prototype.abort = function(reason) {
        this._stream._closed = true;
        if (this._stream._abortFn) {
            this._stream._abortFn(reason);
        }
    };

    WritableStreamDefaultWriter.prototype.releaseLock = function() {
        this._stream._writer = null;
    };

    // WritableStream
    globalThis.WritableStream = function(underlyingSink, strategy) {
        this._closed = false;
        this._written = [];
        this._writer = null;
        this._highWaterMark = (strategy && strategy.highWaterMark) || 1;
        this._writeFn = underlyingSink && underlyingSink.write ? underlyingSink.write.bind(underlyingSink) : null;
        this._closeFn = underlyingSink && underlyingSink.close ? underlyingSink.close.bind(underlyingSink) : null;
        this._abortFn = underlyingSink && underlyingSink.abort ? underlyingSink.abort.bind(underlyingSink) : null;

        if (underlyingSink && underlyingSink.start) {
            underlyingSink.start();
        }
    };

    WritableStream.prototype.getWriter = function() {
        if (this._writer) throw new TypeError('WritableStream already has a writer');
        var writer = new WritableStreamDefaultWriter(this);
        this._writer = writer;
        return writer;
    };

    WritableStream.prototype.abort = function(reason) {
        this._closed = true;
        if (this._abortFn) this._abortFn(reason);
    };

    Object.defineProperty(WritableStream.prototype, 'locked', {
        get: function() { return this._writer !== null; }
    });

    // TransformStreamDefaultController
    function TransformStreamDefaultController(stream) {
        this._stream = stream;
    }

    TransformStreamDefaultController.prototype.enqueue = function(chunk) {
        this._stream._readable._queue.push(chunk);
    };

    TransformStreamDefaultController.prototype.error = function(e) {
        this._stream._readable._errored = true;
        this._stream._readable._error = e;
    };

    TransformStreamDefaultController.prototype.terminate = function() {
        this._stream._readable._closed = true;
    };

    // TransformStream
    globalThis.TransformStream = function(transformer, writableStrategy, readableStrategy) {
        var self = this;
        this._transformer = transformer || {};
        this._controller = new TransformStreamDefaultController(this);

        this._readable = new ReadableStream({}, readableStrategy);
        this._writable = new WritableStream({
            write: function(chunk) {
                if (self._transformer.transform) {
                    self._transformer.transform(chunk, self._controller);
                } else {
                    // Identity transform
                    self._controller.enqueue(chunk);
                }
            },
            close: function() {
                if (self._transformer.flush) {
                    self._transformer.flush(self._controller);
                }
                self._readable._closed = true;
            }
        }, writableStrategy);

        if (this._transformer.start) {
            this._transformer.start(this._controller);
        }
    };

    Object.defineProperty(TransformStream.prototype, 'readable', {
        get: function() { return this._readable; }
    });

    Object.defineProperty(TransformStream.prototype, 'writable', {
        get: function() { return this._writable; }
    });
})();
