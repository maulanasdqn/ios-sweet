// Puts a custom wallpaper on the iOS 16 lock and home screen through
// WallpaperKit + PosterBoard: installs the "SkyIOS" wallpaper collection
// (make-collection.sh; staged at STAGED by the boot-time installer, xnu
// 0110) into /Library/Wallpaper/Collections, makes it the default wallpaper
// (Collections~iphone.plist order, DefaultWallpapers~iphone.plist "default",
// originals kept as .sweet-old), then asks PosterBoard to delete its data
// store so it creates the default poster again. `posterwall undo` restores.
// Run once at boot by sweetrun; then execs the original binary.

#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <objc/message.h>
#include <objc/runtime.h>
#include <sys/sysctl.h>
// libproc (in libSystem; the iOS SDK has no libproc.h)
extern int proc_listpids(uint32_t type, uint32_t typeinfo, void *buffer, int buffersize);
extern int proc_name(int pid, void *buffer, uint32_t buffersize);
#define PROC_ALL_PIDS 1
#include <unistd.h>

#define STAGED @"/private/var/root/wp/C"
#define COLLECTIONS @"/Library/Wallpaper/Collections"
#define OURS COLLECTIONS @"/SkyIOS.wallpaperCollection"
#define INDEX COLLECTIONS @"/Collections~iphone.plist"
#define DEFAULTS @"/Library/Wallpaper/DefaultWallpapers~iphone.plist"
#define OUR_UUID @"5C1E5000-0000-4000-8000-000000009000"

static void say(NSString *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    NSString *s = [[NSString alloc] initWithFormat:fmt arguments:ap];
    va_end(ap);
    int fd = open("/dev/console", O_WRONLY | O_NOCTTY);
    if (fd >= 0) {
        dprintf(fd, "skyios-posterwall: %s\n", s.UTF8String);
        close(fd);
    }
}

static NSString *backup_of(NSString *p) {
    return [p stringByAppendingString:@".sweet-old"];
}

// edit a plist in its own format, keeping the first original as .sweet-old
static BOOL edit_plist(NSString *path, void (^edit)(NSMutableDictionary *)) {
    NSFileManager *fm = [NSFileManager defaultManager];
    if (![fm fileExistsAtPath:backup_of(path)] && ![fm copyItemAtPath:path toPath:backup_of(path) error:nil]) {
        say(@"backup of %@ failed", path);
        return NO;
    }
    NSData *d = [NSData dataWithContentsOfFile:backup_of(path)];
    NSPropertyListFormat fmt = NSPropertyListBinaryFormat_v1_0;
    NSMutableDictionary *p = [NSPropertyListSerialization propertyListWithData:d ?: [NSData data]
                                                                       options:NSPropertyListMutableContainersAndLeaves
                                                                        format:&fmt
                                                                         error:nil];
    if (![p isKindOfClass:[NSMutableDictionary class]]) {
        say(@"%@: not a dictionary", path);
        return NO;
    }
    edit(p);
    NSData *out = [NSPropertyListSerialization dataWithPropertyList:p format:fmt options:0 error:nil];
    BOOL ok = [out writeToFile:path atomically:YES];
    say(@"%@ written: %d", path, ok);
    return ok;
}

static void install(void) {
    NSFileManager *fm = [NSFileManager defaultManager];
    NSError *err = nil;
    [fm removeItemAtPath:OURS error:nil];
    if (![fm copyItemAtPath:STAGED toPath:OURS error:&err]) {
        say(@"copy collection: %@", err.localizedDescription);
        return;
    }
    say(@"collection installed at %@", OURS);
    edit_plist(INDEX, ^(NSMutableDictionary *p) {
        NSMutableArray *order = p[@"order"];
        [order removeObject:OUR_UUID];
        [order insertObject:OUR_UUID atIndex:0];
    });
    edit_plist(DEFAULTS, ^(NSMutableDictionary *p) {
        NSDictionary *ours = @{@"collectionIdentifier" : OUR_UUID, @"wallpaperIdentifier" : @9000};
        p[@"default"] = ours;
        // the phone says it is an iPhone X
        for (NSString *k in @[ @"iPhone10,3", @"iPhone10,6" ])
            [p removeObjectForKey:k];
    });
}

static void undo(void) {
    NSFileManager *fm = [NSFileManager defaultManager];
    [fm removeItemAtPath:OURS error:nil];
    for (NSString *p in @[ INDEX, DEFAULTS ])
        if ([fm fileExistsAtPath:backup_of(p)]) {
            [fm removeItemAtPath:p error:nil];
            [fm moveItemAtPath:backup_of(p) toPath:p error:nil];
        }
    say(@"undo done");
}

static void say_long(NSString *prefix, NSString *text) {
    // one console line per piece: object descriptions contain newlines
    NSArray *words = [text componentsSeparatedByCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]];
    text = [[words filteredArrayUsingPredicate:[NSPredicate predicateWithFormat:@"length > 0"]] componentsJoinedByString:@" "];
    for (NSUInteger off = 0; off < text.length; off += 130)
        say(@"%@%@", off ? @"  .." : prefix, [text substringWithRange:NSMakeRange(off, MIN(130, text.length - off))]);
}

// PRSService completions are (result object, NSError) (disassembly of
// -[PRSService fetchExtensionIdentifiersWithCompletion:]_block_invoke: blr
// with x1 = result, x2 = error, both retained objects)
typedef void (^objerr)(id, id);

static void call(id svc, NSString *selname, NSArray *args, NSString *label) {
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    objerr cb = ^(id a, id b) {
        say_long([label stringByAppendingString:@" -> "], [NSString stringWithFormat:@"%@ | %@", a ?: @"nil", b ?: @"no error"]);
        dispatch_semaphore_signal(done);
    };
    SEL sel = NSSelectorFromString(selname);
    if (args.count == 0)
        ((void (*)(id, SEL, id))objc_msgSend)(svc, sel, cb);
    else if (args.count == 1)
        ((void (*)(id, SEL, id, id))objc_msgSend)(svc, sel, args[0], cb);
    else
        ((void (*)(id, SEL, id, id, id))objc_msgSend)(svc, sel, args[0], args[1], cb);
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC)))
        say(@"%@: no answer in 30 s", label);
}

// probe: what PosterBoard has, then a poster from our collection
static void probe_posterboard(void) {
    if (!dlopen("/System/Library/PrivateFrameworks/PosterBoardServices.framework/PosterBoardServices", RTLD_NOW))
        return;
    id svc = [NSClassFromString(@"PRSService") new];
    call(svc, @"fetchExtensionIdentifiersWithCompletion:", @[], @"extensions");
    call(svc, @"fetchPosterDescriptorsForExtension:completion:", @[ @"com.apple.WallpaperKit.CollectionsPoster" ], @"collections descriptors");
    call(svc, @"fetchPosterConfigurations:", @[], @"configurations");
    call(svc, @"fetchSelectedConfiguration:", @[], @"selected");
}

// apply: CollectionsPoster descriptors refreshed, the one for our wallpaper
// (identifier containing "9000", else the first) made into a poster
// configuration, which is then selected
static id wait_obj(id svc, NSString *selname, NSArray *args, NSString *label) {
    __block id result = nil;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    objerr cb = ^(id a, id b) {
        result = a;
        say_long([label stringByAppendingString:@" -> "], [NSString stringWithFormat:@"%@ | %@", a ?: @"nil", b ?: @"no error"]);
        dispatch_semaphore_signal(done);
    };
    SEL sel = NSSelectorFromString(selname);
    if (args.count == 0)
        ((void (*)(id, SEL, id))objc_msgSend)(svc, sel, cb);
    else if (args.count == 1)
        ((void (*)(id, SEL, id, id))objc_msgSend)(svc, sel, args[0], cb);
    else
        ((void (*)(id, SEL, id, id, id))objc_msgSend)(svc, sel, args[0], args[1], cb);
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_SEC)))
        say(@"%@: no answer in 60 s", label);
    return result;
}

static void apply_poster(void) {
    if (!dlopen("/System/Library/PrivateFrameworks/PosterBoardServices.framework/PosterBoardServices", RTLD_NOW))
        return;
    id svc = [NSClassFromString(@"PRSService") new];
    NSString *ext = @"com.apple.WallpaperKit.CollectionsPoster";
    wait_obj(svc, @"refreshPosterDescriptorsForExtension:completion:", @[ ext ], @"refresh descriptors");
    id descs = wait_obj(svc, @"fetchPosterDescriptorsForExtension:completion:", @[ ext ], @"descriptors");
    NSString *pick = nil;
    for (id d in ([descs respondsToSelector:@selector(countByEnumeratingWithState:objects:count:)] ? descs : @[])) {
        NSString *ident = nil;
        for (NSString *k in @[ @"descriptorIdentifier", @"identifier", @"posterDescriptorIdentifier" ])
            if ([d respondsToSelector:NSSelectorFromString(k)]) {
                ident = ((id(*)(id, SEL))objc_msgSend)(d, NSSelectorFromString(k));
                break;
            }
        say(@"descriptor %@ identifier %@", d, ident);
        if ([ident isKindOfClass:[NSString class]] && (!pick || [ident containsString:@"9000"]))
            pick = ident;
    }
    if (!pick) {
        say(@"no descriptor to use");
        return;
    }
    say(@"using descriptor %@", pick);
    id config = wait_obj(svc, @"createPosterConfigurationForProviderIdentifier:posterDescriptorIdentifier:completion:",
                         @[ ext, pick ], @"create configuration");
    if (config)
        wait_obj(svc, @"updateToSelectedConfiguration:completion:", @[ config ], @"select configuration");
    wait_obj(svc, @"fetchSelectedConfiguration:", @[], @"selected now");
}

// legacy: set the image through SpringBoard's legacy wallpaper service and
// list /var/mobile/Library/SpringBoard (sizes) before and after, to see
// whether SpringBoard manages to write LockBackground.cpbitmap
static void list_sb(NSString *when) {
    NSString *dir = @"/private/var/mobile/Library/SpringBoard";
    NSMutableArray *parts = [NSMutableArray array];
    for (NSString *n in [[[NSFileManager defaultManager] contentsOfDirectoryAtPath:dir error:nil] sortedArrayUsingSelector:@selector(compare:)]) {
        NSDictionary *a = [[NSFileManager defaultManager] attributesOfItemAtPath:[dir stringByAppendingPathComponent:n] error:nil];
        [parts addObject:[NSString stringWithFormat:@"%@=%llu", n, [a fileSize]]];
    }
    say_long([NSString stringWithFormat:@"SpringBoard dir %@: ", when], [parts componentsJoinedByString:@" "]);
}

static void legacy_set(void) {
    list_sb(@"before");
    if (!dlopen("/System/Library/PrivateFrameworks/SpringBoardUIServices.framework/SpringBoardUIServices", RTLD_NOW))
        return;
    void (*setimg)(id, NSInteger) = dlsym(RTLD_DEFAULT, "SBSUIWallpaperSetImageAsWallpaperForLocations");
    Class uiimage = NSClassFromString(@"UIImage");
    id img = ((id(*)(id, SEL, id))objc_msgSend)(uiimage, NSSelectorFromString(@"imageWithContentsOfFile:"),
                                               @"/private/var/root/skyios-wallpaper.img");
    say(@"image %@, set function %p", img, setimg);
    if (setimg && img)
        setimg(img, 3);
    sleep(15);
    list_sb(@"after");
}

// wkprobe: what WallpaperKit (the library CollectionsPoster builds its
// descriptors from) sees: every collection and its wallpaper bundles
static id msg0(id o, NSString *sel) {
    SEL s = NSSelectorFromString(sel);
    return [o respondsToSelector:s] ? ((id(*)(id, SEL))objc_msgSend)(o, s) : nil;
}

static void wallpaperkit_probe(void) {
    if (!dlopen("/System/Library/PrivateFrameworks/WallpaperKit.framework/WallpaperKit", RTLD_NOW)) {
        say(@"dlopen WallpaperKit: %s", dlerror());
        return;
    }
    for (NSString *which in @[ @"defaultManager", @"defaultLegacyManager" ]) {
        id m = msg0(NSClassFromString(@"WKWallpaperRepresentingCollectionsManager"), which);
        NSUInteger n = ((NSUInteger(*)(id, SEL))objc_msgSend)(m, NSSelectorFromString(@"numberOfWallpaperCollections"));
        say(@"%@: %@, %lu collections", which, m, (unsigned long)n);
        for (NSUInteger i = 0; i < n; i++) {
            id c = ((id(*)(id, SEL, NSUInteger))objc_msgSend)(m, NSSelectorFromString(@"wallpaperCollectionAtIndex:"), i);
            NSUInteger k = [c respondsToSelector:NSSelectorFromString(@"numberOfWallpapers")]
                               ? ((NSUInteger(*)(id, SEL))objc_msgSend)(c, NSSelectorFromString(@"numberOfWallpapers"))
                               : 0;
            say_long([NSString stringWithFormat:@"  [%lu] ", (unsigned long)i],
                     [NSString stringWithFormat:@"%@ id %@: %lu wallpapers", c, msg0(c, @"identifier"), (unsigned long)k]);
            for (NSUInteger j = 0; j < k && j < 4; j++) {
                id b = ((id(*)(id, SEL, NSUInteger))objc_msgSend)(c, NSSelectorFromString(@"wallpaperBundleAtIndex:"), j);
                say_long(@"      ", [NSString stringWithFormat:@"%@", b]);
            }
        }
    }
}

// fsprobe: find PosterBoard's data container and list its tree (files with
// sizes), to see where poster configurations and snapshots live
static void fs_probe(void) {
    NSFileManager *fm = [NSFileManager defaultManager];
    // the legacy wallpaper files and inject()'s backups of them
    list_sb(@"fsprobe");
    NSString *bkroot = @"/private/var/mobile/Library/SpringBoard.sweet-legacy-wallpaper";
    for (NSString *d in [fm contentsOfDirectoryAtPath:bkroot error:nil])
        say(@"legacy backup %@: %@", d,
            [[fm contentsOfDirectoryAtPath:[bkroot stringByAppendingPathComponent:d] error:nil] componentsJoinedByString:@" "]);
    // the PosterBoard data store of the legacy and the collections poster
    NSString *store = @"/private/var/mobile/Library/Application Support/PRBPosterExtensionDataStore";
    say(@"store: %@", [[fm contentsOfDirectoryAtPath:store error:nil] componentsJoinedByString:@" "]);
    for (NSString *v in [fm contentsOfDirectoryAtPath:store error:nil]) {
        NSString *exts = [[store stringByAppendingPathComponent:v] stringByAppendingPathComponent:@"Extensions"];
        say(@"%@/Extensions: %@", v, [[fm contentsOfDirectoryAtPath:exts error:nil] componentsJoinedByString:@" "]);
        for (NSString *x in @[ @"com.apple.PaperBoard.LegacyPoster", @"com.apple.WallpaperKit.CollectionsPoster" ]) {
            NSString *d = [exts stringByAppendingPathComponent:x];
            NSDirectoryEnumerator *e = [fm enumeratorAtPath:d];
            int n = 0;
            for (NSString *rel in e) {
                say(@"  %@/%@ %llu", x.pathExtension, rel, e.fileAttributes.fileSize);
                if (++n >= 80)
                    break;
            }
        }
    }
    if (access("/private/var/root/wp/fsprobe-all", F_OK) != 0)
        return;
    // every path under /private/var/mobile whose name mentions posters
    NSDirectoryEnumerator *all = [fm enumeratorAtPath:@"/private/var/mobile"];
    int hits = 0;
    for (NSString *rel in all) {
        if (all.level > 9) {
            [all skipDescendants];
            continue;
        }
        NSString *last = rel.lastPathComponent;
        if ([last rangeOfString:@"oster" options:NSCaseInsensitiveSearch].location == NSNotFound &&
            [last rangeOfString:@"napshot" options:NSCaseInsensitiveSearch].location == NSNotFound)
            continue;
        say(@"hit %@ %llu", rel, all.fileAttributes.fileSize);
        if (++hits >= 60)
            break;
    }
    NSString *base = @"/private/var/mobile/Containers/Data/Application";
    for (NSString *u in [fm contentsOfDirectoryAtPath:base error:nil]) {
        NSString *dir = [base stringByAppendingPathComponent:u];
        NSDictionary *meta = [NSDictionary
            dictionaryWithContentsOfFile:[dir stringByAppendingPathComponent:@".com.apple.mobile_container_manager.metadata.plist"]];
        NSString *ident = meta[@"MCMMetadataIdentifier"];
        if (![ident containsString:@"PosterBoard"] && ![ident containsString:@"Poster"])
            continue;
        say(@"container %@ = %@", u, ident);
        NSDirectoryEnumerator *e = [fm enumeratorAtPath:dir];
        int n = 0;
        for (NSString *rel in e) {
            NSDictionary *a = e.fileAttributes;
            if ([a.fileType isEqualToString:NSFileTypeDirectory] && e.level > 7)
                [e skipDescendants];
            say(@"  %@ %llu", rel, [a.fileType isEqualToString:NSFileTypeDirectory] ? 0 : a.fileSize);
            if (++n >= 150) {
                say(@"  ... (stopped at 150 entries)");
                break;
            }
        }
    }
}

// snap: write the legacy poster's runtime snapshots ourselves (SpringBoard
// cannot render them on this port: nil image + trap in
// -[PBUIImageEncoder saveUIImage:], 0112s/0112z) with PaperBoardUI's own
// encoder and snapshot format, then put the legacy wallpaper files (written
// by SpringBoard in 0112y, moved aside by sbfix) back
#define LEGACY_CFG @"/private/var/mobile/Library/Application Support/PRBPosterExtensionDataStore/59/Extensions/" \
                   @"com.apple.PaperBoard.LegacyPoster/configurations/A8A6A316-259B-4C29-B15C-3801F78E738D/versions/0"

static void chown_mobile(NSString *path) {
    [[NSFileManager defaultManager] setAttributes:@{NSFileOwnerAccountID : @501, NSFileGroupOwnerAccountID : @501}
                                     ofItemAtPath:path
                                            error:nil];
}

static void write_snapshots(void) {
    NSFileManager *fm = [NSFileManager defaultManager];
    for (NSString *v in @[ @"lock", @"home" ]) {
        NSString *meta = [LEGACY_CFG stringByAppendingPathComponent:[NSString stringWithFormat:@"RuntimeSnapshotMetadata-%@.plist", v]];
        say_long([NSString stringWithFormat:@"metadata %@: ", v], [[NSDictionary dictionaryWithContentsOfFile:meta] description] ?: @"(none)");
    }
    if (!dlopen("/System/Library/PrivateFrameworks/PaperBoardUI.framework/PaperBoardUI", RTLD_NOW)) {
        say(@"dlopen PaperBoardUI: %s", dlerror());
        return;
    }
    id fmt = msg0(NSClassFromString(@"PBUIPosterVariantViewController"), @"snapshotFormat");
    say(@"snapshot format %@", fmt);
    if (!fmt)
        return;
    // the image at screen size (1125x2436, aspect fill), scale 1 so the
    // pixels are exactly that
    Class uiimage = NSClassFromString(@"UIImage");
    id src = ((id(*)(id, SEL, id))objc_msgSend)(uiimage, NSSelectorFromString(@"imageWithContentsOfFile:"), @"/private/var/root/skyios-wallpaper.img");
    CGImageRef cg = (__bridge CGImageRef)msg0(src, @"CGImage");
    if (!cg) {
        say(@"no source image");
        return;
    }
    size_t W = 1125, H = 2436;
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGContextRef c = CGBitmapContextCreate(NULL, W, H, 8, 0, cs, kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
    double sw = CGImageGetWidth(cg), sh = CGImageGetHeight(cg), k = fmax(W / sw, H / sh);
    CGContextDrawImage(c, CGRectMake((W - sw * k) / 2, (H - sh * k) / 2, sw * k, sh * k), cg);
    CGImageRef full = CGBitmapContextCreateImage(c);
    id img = ((id(*)(id, SEL, CGImageRef))objc_msgSend)(uiimage, NSSelectorFromString(@"imageWithCGImage:"), full);
    CGImageRelease(full);
    CGContextRelease(c);
    CGColorSpaceRelease(cs);
    for (NSString *v in @[ @"lock", @"home" ]) {
        NSString *path = [LEGACY_CFG stringByAppendingPathComponent:[NSString stringWithFormat:@"RuntimeSnapshot-%@.atx", v]];
        NSString *old = [path stringByAppendingString:@".sweet-old"];
        if ([fm fileExistsAtPath:path] && ![fm fileExistsAtPath:old])
            [fm copyItemAtPath:path toPath:old error:nil];
        id enc = [NSClassFromString(@"PBUIImageEncoder") alloc];
        enc = ((id(*)(id, SEL, id, id))objc_msgSend)(enc, NSSelectorFromString(@"initWithURL:format:"), [NSURL fileURLWithPath:path], fmt);
        NSError *err = nil;
        BOOL ok = ((BOOL(*)(id, SEL, id, NSError **))objc_msgSend)(enc, NSSelectorFromString(@"saveUIImage:error:"), img, &err);
        chown_mobile(path);
        say(@"snapshot %@: saved %d, %llu bytes, %@", v, ok, [[fm attributesOfItemAtPath:path error:nil] fileSize], err ?: @"no error");
    }
    // legacy wallpaper files back from the newest sbfix backup
    NSString *bk = @"/private/var/mobile/Library/SpringBoard.sweet-legacy-wallpaper/70";
    for (NSString *n in @[ @"LockBackground.cpbitmap", @"OriginalLockBackground.cpbitmap", @"LockBackgroundThumbnail.jpg", @"LockBackgroundThumbnaildark.jpg" ]) {
        NSString *to = [@"/private/var/mobile/Library/SpringBoard" stringByAppendingPathComponent:n];
        if ([fm fileExistsAtPath:to])
            continue;
        BOOL ok = [fm copyItemAtPath:[bk stringByAppendingPathComponent:n] toPath:to error:nil];
        chown_mobile(to);
        say(@"legacy %@ restored: %d", n, ok);
    }
}

// inject: SpringBoard gets /usr/lib/skyios/SkyPBFix.dylib (CPU render of
// wallpaper snapshots, skyios-ios/pbfix) through DYLD_INSERT_LIBRARIES in its
// job in launchd's cache (/System/Library/xpc/launchd.plist, read unsigned
// with launchd_unsecure_cache=1; original kept as .sweet-old; dyld honours
// it only with boot-arg sweetdyldvars=1); and the legacy wallpaper files
// (the user's image) come back from the newest sbfix backup. Both take
// effect at the next boot.
#define LAUNCHD_PLIST @"/System/Library/xpc/launchd.plist"
#define PBFIX_DYLIB @"/usr/lib/skyios/SkyPBFix.dylib"

static void restore_legacy(void);

static void inject(void) {
    NSFileManager *fm = [NSFileManager defaultManager];
    NSString *old = [LAUNCHD_PLIST stringByAppendingString:@".sweet-old"];
    if (![fm fileExistsAtPath:old] && ![fm copyItemAtPath:LAUNCHD_PLIST toPath:old error:nil]) {
        say(@"launchd.plist backup failed");
        return;
    }
    NSPropertyListFormat fmt = NSPropertyListBinaryFormat_v1_0;
    NSMutableDictionary *pl = [NSPropertyListSerialization propertyListWithData:[NSData dataWithContentsOfFile:old]
                                                                        options:NSPropertyListMutableContainersAndLeaves
                                                                         format:&fmt
                                                                          error:nil];
    NSMutableDictionary *daemons = pl[@"LaunchDaemons"];
    int found = 0;
    for (NSString *k in daemons) {
        NSMutableDictionary *job = daemons[k];
        if (![job isKindOfClass:[NSDictionary class]] || ![job[@"Label"] isEqual:@"com.apple.SpringBoard"])
            continue;
        NSMutableDictionary *env = job[@"EnvironmentVariables"] ?: [NSMutableDictionary dictionary];
        env[@"DYLD_INSERT_LIBRARIES"] = PBFIX_DYLIB;
        env[@"SKYIOS_TEST"] = @"1"; // does launchd pass job variables at all? (0113o: no DYLD_*)
        job[@"EnvironmentVariables"] = env;
        found++;
        say(@"SpringBoard job %@: environment %@", k, env);
    }
    NSData *out = [NSPropertyListSerialization dataWithPropertyList:pl format:fmt options:0 error:nil];
    BOOL ok = found && [out writeToFile:LAUNCHD_PLIST atomically:YES];
    say(@"launchd.plist: %d SpringBoard job(s), written %d (format %lu)", found, ok, (unsigned long)fmt);
    restore_legacy();
}

// newest legacy wallpaper backup back in place (the files vanished from
// SpringBoard's directory between Nov 16 and 20: black wallpaper, probe116)
static void restore_legacy(void) {
    NSFileManager *fm = [NSFileManager defaultManager];
    NSString *bkroot = @"/private/var/mobile/Library/SpringBoard.sweet-legacy-wallpaper";
    NSString *newest = nil;
    for (NSString *d in [fm contentsOfDirectoryAtPath:bkroot error:nil])
        if ([fm fileExistsAtPath:[[bkroot stringByAppendingPathComponent:d] stringByAppendingPathComponent:@"LockBackground.cpbitmap"]] &&
            (!newest || d.integerValue > newest.integerValue))
            newest = d;
    for (NSString *n in @[ @"LockBackground.cpbitmap", @"OriginalLockBackground.cpbitmap", @"LockBackgroundThumbnail.jpg", @"LockBackgroundThumbnaildark.jpg" ]) {
        NSString *to = [@"/private/var/mobile/Library/SpringBoard" stringByAppendingPathComponent:n];
        if (!newest || [fm fileExistsAtPath:to])
            continue;
        BOOL c = [fm copyItemAtPath:[[bkroot stringByAppendingPathComponent:newest] stringByAppendingPathComponent:n] toPath:to error:nil];
        chown_mobile(to);
        say(@"legacy %@ restored from %@: %d", n, newest, c);
    }
}

static void uninject(void) {
    NSFileManager *fm = [NSFileManager defaultManager];
    NSString *old = [LAUNCHD_PLIST stringByAppendingString:@".sweet-old"];
    if ([fm fileExistsAtPath:old]) {
        [fm removeItemAtPath:LAUNCHD_PLIST error:nil];
        BOOL ok = [fm copyItemAtPath:old toPath:LAUNCHD_PLIST error:nil];
        say(@"launchd.plist restored: %d", ok);
    }
}

// PosterBoard creates the default poster when its data store is empty
static void reset_posterboard(void) {
    if (!dlopen("/System/Library/PrivateFrameworks/PosterBoardServices.framework/PosterBoardServices", RTLD_NOW)) {
        say(@"dlopen PosterBoardServices: %s", dlerror());
        return;
    }
    id svc = [NSClassFromString(@"PRSService") new];
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    // completion types not known: log the first two words raw
    void (^cb)(uintptr_t, uintptr_t) = ^(uintptr_t a, uintptr_t b) {
        say(@"deleteDataStore -> %#lx %#lx", (unsigned long)a, (unsigned long)b);
        dispatch_semaphore_signal(done);
    };
    ((void (*)(id, SEL, id))objc_msgSend)(svc, NSSelectorFromString(@"deleteDataStoreWithCompletion:"), cb);
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC)))
        say(@"deleteDataStore: no answer in 30 s");
}

// jetsam: the job we run in (DumpPanic) has a small per-process memory
// limit; the ATX/ASTC encode of the snapshots exceeded it (0113g/h: SIGKILL,
// reason namespace 1 = jetsam, code 7 = per-process limit). Root may raise it.
// (the kernel is built with the macOS configuration: no CONFIG_JETSAM, so
// MEMORYSTATUS_CMD_SET_JETSAM_TASK_LIMIT is absent; SET_MEMLIMIT_PROPERTIES is
// there)
extern int memorystatus_control(uint32_t command, int32_t pid, uint32_t flags, void *buffer, size_t buffersize);
#define MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES 7
struct sky_memlimit {
    int32_t active;
    uint32_t active_attr;
    int32_t inactive;
    uint32_t inactive_attr;
};

int main(int argc, char **argv, char **envp, char **apple) {
    struct sky_memlimit ml = {2048, 0, 2048, 0};
    int lim = memorystatus_control(MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES, getpid(), 0, &ml, sizeof(ml));
    say(@"memory limit -> 2048 MB: %d (errno %d)", lim, lim ? errno : 0);
    @autoreleasepool {
        // one mode per boot, from /private/var/root/wp/mode (rewritten by the
        // boot-time installer; the old per-mode marker files stayed on disk
        // and picked the wrong mode, 0113p)
        NSString *mode = [[NSString stringWithContentsOfFile:@"/private/var/root/wp/mode" encoding:NSUTF8StringEncoding error:nil]
            stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]] ?: @"";
        say(@"mode '%@'", mode);
        if (argc > 1 && strcmp(argv[1], "undo") == 0) {
            undo();
        } else if ([mode isEqualToString:@"readlog"]) {
            // SkyPBFix's log from SpringBoard (it cannot write to the console)
            sleep(230);
            NSString *log = [NSString stringWithContentsOfFile:@"/private/var/mobile/Library/Logs/SkyPBFix.log" encoding:NSUTF8StringEncoding error:nil];
            for (NSString *l in [log componentsSeparatedByString:@"\n"])
                if (l.length)
                    say(@"SkyPBFix log: %@", l);
            say(@"SkyPBFix log: %lu bytes", (unsigned long)log.length);
            // did launchd pass the variable? SpringBoard's env from KERN_PROCARGS2
            int pids[2048];
            int n = proc_listpids(PROC_ALL_PIDS, 0, pids, sizeof(pids)) / (int)sizeof(int);
            for (int i = 0; i < n; i++) {
                char name[64] = "";
                proc_name(pids[i], name, sizeof(name));
                if (strcmp(name, "SpringBoard") != 0)
                    continue;
                int mib[3] = {CTL_KERN, KERN_PROCARGS2, pids[i]};
                static char buf[65536];
                size_t len = sizeof(buf);
                if (sysctl(mib, 3, buf, &len, NULL, 0) != 0) {
                    say(@"procargs %d: errno %d", pids[i], errno);
                    continue;
                }
                int argc2;
                memcpy(&argc2, buf, 4);
                char *p = buf + 4, *end = buf + len;
                p += strlen(p) + 1; // exec path
                while (p < end && !*p)
                    p++;
                for (int a = 0; a < argc2 && p < end; a++)
                    p += strlen(p) + 1;
                for (; p < end && *p; p += strlen(p) + 1)
                    if (strncmp(p, "DYLD", 4) == 0 || strncmp(p, "SKYIOS", 6) == 0 || strncmp(p, "PATH", 4) == 0)
                        say(@"SpringBoard[%d] env %s", pids[i], p);
                say(@"SpringBoard[%d] env scanned", pids[i]);
            }
        } else if ([mode isEqualToString:@"inject"]) {
            inject();
        } else if ([mode isEqualToString:@"restore"]) {
            restore_legacy();
            list_sb(@"restored");
        } else if ([mode isEqualToString:@"uninject"]) {
            uninject();
        } else if ([mode isEqualToString:@"snap"]) {
            write_snapshots();
        } else if ([mode isEqualToString:@"fsprobe"]) {
            fs_probe();
        } else if ([mode isEqualToString:@"wkprobe"]) {
            wallpaperkit_probe();
        } else if ([mode isEqualToString:@"legacy"]) {
            say(@"legacy mode: waiting 90 s for SpringBoard");
            sleep(90);
            legacy_set();
        } else if ([mode isEqualToString:@"apply"]) {
            say(@"apply mode: waiting 90 s for PosterBoard");
            sleep(90);
            apply_poster();
        } else if ([mode isEqualToString:@"probe"]) {
            say(@"probe mode: waiting 90 s for PosterBoard");
            sleep(90);
            probe_posterboard();
        } else if ([mode isEqualToString:@"install"]) {
            install();
            say(@"waiting 90 s for PosterBoard");
            sleep(90);
            reset_posterboard();
        } else {
            say(@"unknown mode, nothing done");
        }
    }
    for (char **a = apple; a && *a; a++)
        if (strncmp(*a, "executable_path=", 16) == 0)
            execve(*a + 16, argv, envp);
    return 0;
}
