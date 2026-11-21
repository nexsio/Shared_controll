/* Offener Nordic-Bootloader: MBR belegt 0x0..0x1000. Obergrenze 0xE0000 ist konservativ, Bootloader-Start des Sticks ist nicht verifiziert. */
MEMORY
{
  FLASH : ORIGIN = 0x00001000, LENGTH = 0xDF000
  RAM   : ORIGIN = 0x20000000, LENGTH = 0x40000
}
