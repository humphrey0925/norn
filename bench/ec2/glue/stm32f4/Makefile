# Generated in the manner of STM32CubeMX: a GCC build of the STM32F407 HAL.
TARGET = stm32f4
BUILD_DIR = build

C_SOURCES = $(wildcard Drivers/STM32F4xx_HAL_Driver/Src/*.c) \
Drivers/CMSIS/Device/ST/STM32F4xx/Source/system_stm32f4xx.c

PREFIX = arm-none-eabi-
CC = $(PREFIX)gcc
AS = $(PREFIX)gcc -x assembler-with-cpp
CP = $(PREFIX)objcopy
SZ = $(PREFIX)size

CPU = -mcpu=cortex-m4
FPU = -mfpu=fpv4-sp-d16
FLOAT-ABI = -mfloat-abi=hard
MCU = $(CPU) -mthumb $(FPU) $(FLOAT-ABI)

C_DEFS =  \
-DUSE_HAL_DRIVER \
-DUSE_FULL_LL_DRIVER \
-DSTM32F407xx

C_INCLUDES =  \
-ICore/Inc \
-IDrivers/STM32F4xx_HAL_Driver/Inc \
-IDrivers/CMSIS/Device/ST/STM32F4xx/Include \
-IDrivers/CMSIS/Include

CFLAGS = $(MCU) $(C_DEFS) $(C_INCLUDES) -O2 -Wall -fdata-sections -ffunction-sections

all: $(BUILD_DIR)/$(TARGET).elf
