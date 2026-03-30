// k6/net/grpc JavaScript shim
(function() {
    var grpc = {
        // gRPC status codes
        StatusOK: 0,
        StatusCancelled: 1,
        StatusUnknown: 2,
        StatusInvalidArgument: 3,
        StatusDeadlineExceeded: 4,
        StatusNotFound: 5,
        StatusAlreadyExists: 6,
        StatusPermissionDenied: 7,
        StatusResourceExhausted: 8,
        StatusFailedPrecondition: 9,
        StatusAborted: 10,
        StatusOutOfRange: 11,
        StatusUnimplemented: 12,
        StatusInternal: 13,
        StatusUnavailable: 14,
        StatusDataLoss: 15,
        StatusUnauthenticated: 16,

        Client: function() {
            this._connId = null;

            this.connect = function(address, params) {
                params = params || {};
                var paramsJson = JSON.stringify(params);
                this._connId = __grpc_connect(String(address), paramsJson);
            };

            this.invoke = function(method, request, params) {
                if (!this._connId) {
                    throw new Error('gRPC client not connected. Call connect() first.');
                }
                params = params || {};
                var requestJson = JSON.stringify(request || {});
                var metadataJson = JSON.stringify(params.metadata || {});
                var resultJson = __grpc_invoke(this._connId, String(method), requestJson, metadataJson);
                return JSON.parse(resultJson);
            };

            this.close = function() {
                if (this._connId) {
                    __grpc_close(this._connId);
                    this._connId = null;
                }
            };
        }
    };

    globalThis.grpc = grpc;
})();
