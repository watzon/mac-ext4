// Kernel-offloaded I/O conformance for Ext4Volume.
//
// FSKit calls completeIOForFile:... with status == nil after a successful
// transfer, although the SDK marks the parameter nonnull. Swift cannot take
// that call: its thunk crashes converting the nil NSError before any Swift
// code runs. So this method is Objective-C and forwards plain values to
// Swift. The other protocol methods are Swift methods with the protocol's
// selectors (Ext4Volume.swift).

#import <FSKit/FSKit.h>

#import "Ext4FS-Swift.h"

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

@interface Ext4Volume (KernelOffloadedIO) <FSVolumeKernelOffloadedIOOperations>
@end

@implementation Ext4Volume (KernelOffloadedIO)

- (void)completeIOForFile:(FSItem *)file
                   offset:(off_t)offset
                   length:(size_t)length
                   status:(NSError *)status
                    flags:(FSCompleteIOFlags)flags
              operationID:(FSOperationID)operationID
             replyHandler:(void (^)(NSError *_Nullable error))reply
{
    BOOL succeeded = status == nil || status.code == 0;
    [self ext4CompleteIOFor:file
                     offset:offset
                     length:(NSInteger)length
                  succeeded:succeeded
                      write:(flags & FSCompleteIOFlagsWrite) != 0
               replyHandler:reply];
}

@end

#pragma clang diagnostic pop
