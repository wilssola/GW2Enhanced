//! Test fixture: the pass structure of one ordinary game frame (21 render passes, a 3440x1440 screen), as the recorder
//! writes it, with the pixel-shader hashes per pass. The classifier, the recorder and the draw diagnostic are tested on it.
//! The frame itself is described in `docs/frame-structure.md`.

pub const FRAME_PASSES: &str = r#"===== FRAME 4500 : 21 segments =====
#0  DS 2048x2048 R24G8_TYPELESS
     draws=10 dispatches=0
       ps (no ps)                                                      x5
       ps ps#ca3dd842                                                  x4
       ps ps#f43b8d5f                                                  x1
#1  RT0 512x512 R16G16_FLOAT
     draws=2 dispatches=0
       ps ps#2ed51259                                                  x1
       ps ps#cc2ca5da                                                  x1
#2  DS 6144x2048 R24G8_TYPELESS
     draws=1489 dispatches=0
       ps (no ps)                                                      x745
       ps ps#01234052                                                  x5
       ps ps#034e033f                                                  x2
       ps ps#0a7084b5                                                  x20
       ps ps#1a5c63b6                                                  x4
       ps ps#1fd9d544                                                  x44
       ps ps#2ef82c27                                                  x20
       ps ps#3c097f77                                                  x22
#3  RT0 3440x1440 B8G8R8A8_UNORM | DS 3440x1440 R24G8_TYPELESS
     draws=932 dispatches=0
       ps ps#035701f6                                                  x94
       ps ps#03d964fa                                                  x2
       ps ps#0440653f                                                  x5
       ps ps#0454c4d3                                                  x7
       ps ps#0541b9bc                                                  x2
       ps ps#079a77e9                                                  x5
       ps ps#095f074c                                                  x13
       ps ps#09d920e5                                                  x1
#4  RT0 3440x1440 R32_FLOAT
     draws=1 dispatches=0
       ps ps#369b6e3b                                                  x1
#5  RT0 1720x720 R32_FLOAT
     draws=1 dispatches=0
       ps ps#97934490                                                  x1
#6  RT0 1720x720 R16G16_UNORM
     draws=3 dispatches=0
       ps ps#15ec44f5                                                  x1
       ps ps#7d6887d9                                                  x1
       ps ps#ca787cbe                                                  x1
#7  RT0 3440x1440 R16_FLOAT
     draws=1 dispatches=0
       ps ps#68936927                                                  x1
#8  RT0 3440x1440 B8G8R8A8_UNORM
     draws=48 dispatches=0
       ps ps#625d33af                                                  x46
       ps ps#c4dee1ff                                                  x1
       ps ps#ee40fa8c                                                  x1
#9  RT0 3440x1440 B8G8R8A8_UNORM | DS 3440x1440 R24G8_TYPELESS
     draws=728 dispatches=0
       ps ps#021cd330                                                  x21
       ps ps#07f653d4                                                  x11
       ps ps#0bea5335                                                  x4
       ps ps#0e519d11                                                  x2
       ps ps#15410792                                                  x1
       ps ps#1766d348                                                  x4
       ps ps#18bff384                                                  x4
       ps ps#1b64df37                                                  x9
#10  RT0 860x360 B8G8R8A8_UNORM
     draws=1 dispatches=0
       ps ps#2ed4a75f                                                  x1
#11  RT0 215x215 B8G8R8A8_UNORM
     draws=1 dispatches=0
       ps ps#2ed4a75f                                                  x1
#12  RT0 1x1 R16G16B16A16_FLOAT
     draws=1 dispatches=0
       ps ps#fbea4f58                                                  x1
#13  RT0 3440x1440 B8G8R8A8_UNORM
     draws=3 dispatches=0
       ps ps#4340ca88                                                  x1
       ps ps#56875aa2                                                  x1
       ps ps#f5cd4832                                                  x1
#14  RT0 860x360 B8G8R8A8_UNORM
     draws=5 dispatches=0
       ps ps#14b1e90e                                                  x1
       ps ps#b73c3be3                                                  x4
#15  RT0 3440x1440 B8G8R8A8_UNORM
     draws=1 dispatches=0
       ps ps#37c5610f                                                  x1
#16  RT0 3440x1440 B8G8R8A8_UNORM | DS 3440x1440 R24G8_TYPELESS
     draws=3 dispatches=0
       ps ps#342c9cb4                                                  x1
       ps ps#833ba6e3                                                  x1
       ps ps#dbd1a9c1                                                  x1
#17  RT0 3440x1440 B8G8R8A8_UNORM
     draws=1 dispatches=0
       ps ps#c0ad1686                                                  x1
#18  RT0 3440x1440 B8G8R8A8_UNORM | DS 3440x1440 R24G8_TYPELESS
     draws=301 dispatches=0
       ps ps#36e6e263                                                  x16
       ps ps#55788607                                                  x3
       ps ps#5df6e342                                                  x1
       ps ps#626d6f2c                                                  x2
       ps ps#77454743                                                  x1
       ps ps#7949b6ba                                                  x17
       ps ps#834cd21c                                                  x2
       ps ps#8546e280                                                  x179
#19  RT0 512x512 B8G8R8A8_UNORM | DS 512x512 R24G8_TYPELESS
     draws=27 dispatches=0
       ps ps#2915a70e                                                  x1
       ps ps#8546e280                                                  x10
       ps ps#899eaebb                                                  x15
       ps ps#a785fc80                                                  x1
#20  RT0 3440x1440 R8G8B8A8_UNORM | DS 3440x1440 R24G8_TYPELESS
     draws=1 dispatches=0
       ps ps#afdc14cb                                                  x1

[arming frame capture at frame 5247]

"#;
