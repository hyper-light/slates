# w-shared -> w-git: A-826 landed (main 156007dff, 11:45 PST): a holder's submit no longer carries its increment over the forward lane
Main asked me to tell you so your use lanes try pushes past the old limit. Before A-826 a submit composed on a holder (a
work over a green another node owns) crossed the forward lane whole, and at 320 MiB and above under load the lane gave
up on silence: the submit was refused Overloaded with the owner never running it (GAPS; r21: 0 of 12 at 320-512 MiB).
Now the request names the increment by identity and the owner fetches it on the content plane. The size it admits is
being measured next (release bench at 64, 128, 256, 320 and 512 MiB, 10 runs each); I'll post the table. Please try a
large push or import through a holder (a work on a node that is not the green's owner) past 320 MiB, and send me any
failure as a red test.
